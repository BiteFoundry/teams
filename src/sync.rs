//! Adaptive 2-lane background synchronization engine for Microsoft Teams (Fast Interactive Lane + Background Poll Lane).

use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use futures_util::StreamExt;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{mpsc, watch},
    time::{self, MissedTickBehavior},
};

use crate::{
    config::AppConfig,
    graph::GraphClient,
    models::{
        ConversationKind, ConversationSummary, ConversationTarget, CoreEvent, MessageItem,
        NotificationCandidate, PresenceStatus, SyncCursor, TeamSummary, POLL_RETRY_SUMMARY,
    },
    storage::Storage,
};

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;
const AUTH_EXPIRY_PAUSE_SECS: u64 = 30;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;
const MAX_POLLED_PRESENCE_PEERS: usize = 60;
const MAX_TIER2_BATCH_SUBREQS: usize = 20;
const ROLLING_CHAT_REVALIDATE_BATCH: usize = 5;
const MIN_ROLLING_CHAT_SLOTS: usize = 2;
const MAX_REFRESH_CHANNELS: usize = 20;
const BACKGROUND_CHANNEL_BATCH_SIZE: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum WindowFocusState {
    Unknown = 0,
    Focused = 1,
    Unfocused = 2,
}

impl WindowFocusState {
    fn from_u8(val: u8) -> Self {
        match val {
            1 => Self::Focused,
            2 => Self::Unfocused,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatPollCacheEntry {
    last_msg_id: Option<String>,
    last_updated_at: Option<String>,
    last_msg_preview: Option<String>,
    title: String,
    unread_count: i32,
}

type ChatPollCache = HashMap<String, ChatPollCacheEntry>;
type PresencePollCache = HashMap<String, (String, String)>;

pub(crate) struct SyncSharedState {
    epoch: Instant,
    last_activity_ms: AtomicU64,
    activity_seq: AtomicU64,
    auth_pause_until_ms: AtomicU64,
    window_focus: AtomicU8,
}

impl SyncSharedState {
    pub(crate) fn new() -> Self {
        Self::with_epoch(Instant::now())
    }

    pub(crate) fn with_epoch(epoch: Instant) -> Self {
        Self {
            epoch,
            last_activity_ms: AtomicU64::new(0),
            activity_seq: AtomicU64::new(0),
            auth_pause_until_ms: AtomicU64::new(0),
            window_focus: AtomicU8::new(WindowFocusState::Unknown as u8),
        }
    }

    pub(crate) fn record_activity(&self, now: Instant) {
        let ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        self.last_activity_ms.store(ms, Ordering::Release);
        self.activity_seq.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn idle_snapshot(&self, now: Instant) -> (Duration, u64) {
        let seq = self.activity_seq.load(Ordering::Acquire);
        let last_ms = self.last_activity_ms.load(Ordering::Acquire);
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        (Duration::from_millis(now_ms.saturating_sub(last_ms)), seq)
    }

    pub(crate) fn window_focus(&self) -> WindowFocusState {
        WindowFocusState::from_u8(self.window_focus.load(Ordering::Acquire))
    }

    pub(crate) fn set_window_focus(&self, state: WindowFocusState) -> WindowFocusState {
        let prev = self.window_focus.swap(state as u8, Ordering::AcqRel);
        WindowFocusState::from_u8(prev)
    }

    pub(crate) fn pause_auth(&self, now: Instant, duration: Duration) {
        let until_ms = (now.saturating_duration_since(self.epoch) + duration).as_millis() as u64;
        self.auth_pause_until_ms
            .store(until_ms.max(1), Ordering::Release);
    }

    pub(crate) fn clear_auth_pause(&self) {
        self.auth_pause_until_ms.store(0, Ordering::Release);
    }

    pub(crate) fn is_auth_paused(&self, now: Instant) -> bool {
        let until_ms = self.auth_pause_until_ms.load(Ordering::Acquire);
        if until_ms == 0 {
            return false;
        }
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        now_ms < until_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncErrorKind {
    Forbidden403,
    Unauthorized401,
    /// The response status was 404: the target (a deleted channel, a left team) is gone.
    NotFound404,
    Transient,
}

fn extract_error_detail_for_status(lower: &str) -> &str {
    if let Some(idx) = lower.find("graph api error (") {
        let rest = &lower[idx..];
        if let Some((_, tail)) = rest.split_once("): ") {
            return tail;
        }
        return rest;
    }
    lower
}

pub(crate) fn classify_sync_error(err: &anyhow::Error) -> SyncErrorKind {
    let full = format!("{err:#}");
    // The parsed response status decides first; a "403"/"401" inside an id, a URL or the
    // error body must not change the class of a different response.
    match crate::graph::extract_graph_http_status(&full) {
        Some(403) => return SyncErrorKind::Forbidden403,
        Some(401) => return SyncErrorKind::Unauthorized401,
        Some(404) => return SyncErrorKind::NotFound404,
        _ => {}
    }
    let lower = full.to_ascii_lowercase();

    if lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient")
        || lower.contains("aclcheckfailed")
    {
        return SyncErrorKind::Forbidden403;
    }

    if lower.contains("unauthorized") || lower.contains("token expired") {
        return SyncErrorKind::Unauthorized401;
    }

    SyncErrorKind::Transient
}

pub(crate) fn is_expired_delta_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    let detail = extract_error_detail_for_status(&lower);
    detail.contains("410")
        || detail.contains("400")
        || lower.contains("resyncrequired")
        || lower.contains("syncstatenotfound")
}

#[derive(Debug)]
pub(crate) struct ErrorBackoffState {
    base_ms: u64,
    consecutive_failures: u32,
    next_retry_at: Option<Instant>,
    degraded: bool,
}

impl ErrorBackoffState {
    pub(crate) fn new(base_ms: u64) -> Self {
        Self {
            base_ms: base_ms.max(1),
            consecutive_failures: 0,
            next_retry_at: None,
            degraded: false,
        }
    }

    pub(crate) fn compute_backoff_ms(base_ms: u64, failures: u32, jitter_factor: f64) -> u64 {
        let shift = failures.min(6);
        let raw_ms = base_ms
            .saturating_mul(1u64 << shift)
            .min(MAX_ERROR_BACKOFF_MS);
        let jittered = (raw_ms as f64 * jitter_factor).round() as u64;
        jittered.clamp(1, MAX_ERROR_BACKOFF_MS)
    }

    /// Records a transient failure and returns `true` iff this transitions into degraded state.
    pub(crate) fn record_transient_failure(&mut self, now: Instant) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let jitter = rand::thread_rng().gen_range(0.8_f64..=1.2_f64);
        let delay_ms = Self::compute_backoff_ms(self.base_ms, self.consecutive_failures, jitter);
        self.next_retry_at = Some(now + Duration::from_millis(delay_ms));
        if !self.degraded {
            self.degraded = true;
            true
        } else {
            false
        }
    }

    /// Resets failure counter and returns `true` iff this transitions out of degraded state.
    pub(crate) fn record_success(&mut self) -> bool {
        self.consecutive_failures = 0;
        self.next_retry_at = None;
        if self.degraded {
            self.degraded = false;
            true
        } else {
            false
        }
    }

    pub(crate) fn reset_window(&mut self) {
        self.consecutive_failures = 0;
        self.next_retry_at = None;
    }

    pub(crate) fn is_backed_off(&self, now: Instant) -> bool {
        self.next_retry_at.is_some_and(|until| now < until)
    }

    pub(crate) fn is_degraded(&self) -> bool {
        self.degraded
    }
}

pub(crate) fn idle_multiplier(idle_duration: Duration) -> u64 {
    let secs = idle_duration.as_secs();
    if secs >= DEEP_IDLE_THRESHOLD_SECS {
        10
    } else if secs >= IDLE_THRESHOLD_SECS {
        4
    } else {
        1
    }
}

pub(crate) fn effective_active_idle_multiplier(
    window_focus: WindowFocusState,
    idle_duration: Duration,
) -> u64 {
    match window_focus {
        WindowFocusState::Focused | WindowFocusState::Unknown => 1,
        WindowFocusState::Unfocused => idle_multiplier(idle_duration).max(4),
    }
}

#[derive(Debug, Default)]
pub(crate) struct PeriodicBackoffTracker {
    tick_count: u64,
    last_activity_seq: u64,
}

impl PeriodicBackoffTracker {
    pub(crate) fn on_tick(&mut self, idle_duration: Duration, activity_seq: u64) -> bool {
        self.on_tick_with_multiplier(idle_multiplier(idle_duration), activity_seq)
    }

    pub(crate) fn on_tick_with_multiplier(&mut self, mult: u64, activity_seq: u64) -> bool {
        if activity_seq != self.last_activity_seq {
            self.last_activity_seq = activity_seq;
            self.tick_count = 0;
        }
        if mult <= 1 {
            self.tick_count = 0;
            return true;
        }
        self.tick_count = self.tick_count.saturating_add(1);
        self.tick_count.is_multiple_of(mult)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTarget {
    pub target: Option<ConversationTarget>,
    pub thread_root_id: Option<String>,
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    nudge_tx: mpsc::UnboundedSender<SyncCommand>,
    shared: Arc<SyncSharedState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncCommand {
    RefreshConversations,
    SyncActiveNow,
    SyncChannelHistory {
        team_id: String,
        channel_id: String,
    },
    NotifyUserActivity,
    SetWindowFocus {
        focused: bool,
    },
    Shutdown,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        nudge_tx: mpsc::UnboundedSender<SyncCommand>,
        _legacy_background_all: bool,
    ) -> Self {
        Self {
            focus_tx,
            nudge_tx,
            shared: Arc::new(SyncSharedState::new()),
        }
    }

    pub fn set_focus(&self, target: Option<ConversationTarget>, thread_root_id: Option<String>) {
        self.shared.record_activity(Instant::now());
        let _ = self.focus_tx.send(FocusTarget {
            target,
            thread_root_id,
        });
    }

    pub fn request(&self, cmd: SyncCommand) {
        let _ = self.nudge_tx.send(cmd);
    }

    pub fn send(&self, cmd: SyncCommand) {
        self.request(cmd);
    }

    pub fn notify_activity(&self) {
        self.request(SyncCommand::NotifyUserActivity);
    }

    pub fn set_window_focus(&self, focused: bool) {
        self.request(SyncCommand::SetWindowFocus { focused });
    }

    pub fn reset_window_focus(&self) {
        self.shared.set_window_focus(WindowFocusState::Unknown);
    }
}

async fn run_storage<F, T>(storage: &Storage, f: F) -> Result<T>
where
    F: FnOnce(&Storage) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let st = storage.clone();
    tokio::task::spawn_blocking(move || f(&st))
        .await
        .map_err(|e| anyhow::anyhow!("Storage blocking task failed: {e}"))?
}

fn compute_messages_signature(messages: &[MessageItem]) -> u64 {
    let mut hasher = DefaultHasher::new();
    messages.len().hash(&mut hasher);
    for m in messages {
        m.msg_id.hash(&mut hasher);
        m.last_modified_at.hash(&mut hasher);
        m.deleted.hash(&mut hasher);
        m.reply_count.hash(&mut hasher);
        m.body_plain.hash(&mut hasher);
        m.subject.hash(&mut hasher);
        m.attachments.len().hash(&mut hasher);
        m.mentions.len().hash(&mut hasher);
        for mt in &m.mentions {
            mt.id.hash(&mut hasher);
            mt.user_id.hash(&mut hasher);
            mt.display_name.hash(&mut hasher);
        }
        m.reactions.len().hash(&mut hasher);
        for r in &m.reactions {
            r.reaction_type.hash(&mut hasher);
            r.count.hash(&mut hasher);
            r.me_reacted.hash(&mut hasher);
            r.users.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn filter_changed_messages_ref(messages: &[MessageItem], changed_ids: &[String]) -> Vec<MessageItem> {
    if changed_ids.is_empty() {
        return Vec::new();
    }
    messages
        .iter()
        .filter(|m| changed_ids.iter().any(|id| id == &m.msg_id))
        .cloned()
        .collect()
}

fn filter_changed_messages(messages: Vec<MessageItem>, changed_ids: &[String]) -> Vec<MessageItem> {
    if changed_ids.is_empty() {
        return Vec::new();
    }
    messages
        .into_iter()
        .filter(|m| changed_ids.iter().any(|id| id == &m.msg_id))
        .collect()
}

async fn current_token_fingerprint(graph: &GraphClient) -> String {
    match graph.auth().current_tokens().await {
        Some(tokens) => {
            let mut hasher = Sha256::new();
            hasher.update(tokens.access_token.as_bytes());
            crate::hex_encode(hasher.finalize())
        }
        None => String::new(),
    }
}

/// Performs an initial bootstrap sync (chats + joined teams/channels) into SQLite.
pub async fn bootstrap_conversations(graph: &GraphClient, storage: &Storage) -> Result<()> {
    let (chats_res, teams_res) = tokio::join!(
        graph.list_chats_page_detailed(50, None),
        graph.list_joined_teams_with_channels(),
    );
    let page = chats_res?;
    run_storage(storage, move |st| {
        st.upsert_conversations_with_null_topics(&page.chats, &page.explicit_null_topic_keys)?;
        save_initial_chats_list_cursor(st, page.next_link)
    })
    .await?;
    if let Ok(fetched) = teams_res {
        let _ = run_storage(storage, move |st| st.upsert_joined_teams(&fetched)).await;
    }
    Ok(())
}

fn handle_lane1_error(
    err: &anyhow::Error,
    focus: &FocusTarget,
    shared: &SyncSharedState,
    restricted_convs: &mut HashSet<String>,
    err_backoff: &mut ErrorBackoffState,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) {
    match classify_sync_error(err) {
        SyncErrorKind::Forbidden403 | SyncErrorKind::NotFound404 => {
            if let Some(target) = &focus.target {
                restricted_convs.insert(target.conv_key());
            }
        }
        SyncErrorKind::Unauthorized401 => {
            shared.pause_auth(Instant::now(), Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
        }
        SyncErrorKind::Transient => {
            if err_backoff.record_transient_failure(Instant::now()) {
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: false,
                    summary: POLL_RETRY_SUMMARY.to_string(),
                });
            }
        }
    }
}

enum ActiveSyncRunOutcome {
    Completed(Result<()>),
    FocusChanged,
    Shutdown,
}

#[allow(clippy::too_many_arguments)]
async fn run_active_sync_cancellable(
    focus: &FocusTarget,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    last_active_sig: &mut HashMap<String, u64>,
    is_periodic: bool,
    focus_rx: &mut watch::Receiver<FocusTarget>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> ActiveSyncRunOutcome {
    let check_focus_rx = focus_rx.clone();
    tokio::select! {
        biased;
        _ = shutdown_rx.changed() => {
            if *shutdown_rx.borrow() {
                ActiveSyncRunOutcome::Shutdown
            } else {
                ActiveSyncRunOutcome::Completed(Ok(()))
            }
        }
        changed = focus_rx.changed() => {
            if changed.is_err() {
                ActiveSyncRunOutcome::Shutdown
            } else {
                ActiveSyncRunOutcome::FocusChanged
            }
        }
        res = sync_active_focus_inner(
            focus,
            graph,
            storage,
            event_tx,
            last_active_sig,
            is_periodic,
            Some(&check_focus_rx),
        ) => ActiveSyncRunOutcome::Completed(res),
    }
}

/// Spawns the two-lane background sync engine (Fast Interactive Lane + Background Poll Lane)
/// and returns a `SyncController` handle.
pub fn spawn_sync_engine(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, mut focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (bg_tx, mut bg_rx) = mpsc::unbounded_channel::<SyncCommand>();

    let active_ms = config.active_poll_ms.max(250);
    let chat_ms = config.chat_poll_ms.max(500);
    let channel_ms = config.channel_poll_ms.max(1_000);
    let resume_min_gap = Duration::from_millis((active_ms / 2).clamp(250, 1_500));
    let config_pinned: HashSet<String> = config.pinned_convs.iter().cloned().collect();
    let shared = Arc::new(SyncSharedState::new());

    // Lane 1: Fast Interactive Lane (active conversation/thread polling + immediate commands)
    let fast_graph = graph.clone();
    let fast_storage = storage.clone();
    let fast_event_tx = event_tx.clone();
    let mut fast_shutdown_rx = shutdown_rx.clone();
    let fast_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        let mut active_tick = time::interval(Duration::from_millis(active_ms));
        active_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut active_backoff = PeriodicBackoffTracker::default();
        let mut restricted_convs: HashSet<String> = HashSet::new();
        let mut last_active_sig: HashMap<String, u64> = HashMap::new();
        let mut active_err_backoff = ErrorBackoffState::new(active_ms);
        let mut last_active_sync_at: Option<Instant> = None;
        let mut pending_focus_sync = false;

        // Consume initial immediate tick so startup isn't flooded simultaneously before focus/commands arrive.
        active_tick.tick().await;

        loop {
            if *fast_shutdown_rx.borrow() {
                break;
            }
            if pending_focus_sync {
                pending_focus_sync = false;
                let now = Instant::now();
                fast_shared.record_activity(now);
                let focus = focus_rx.borrow_and_update().clone();
                let is_restricted = focus
                    .target
                    .as_ref()
                    .is_some_and(|t| restricted_convs.contains(&t.conv_key()));
                if !is_restricted
                    && !fast_shared.is_auth_paused(now)
                    && !active_err_backoff.is_backed_off(now)
                {
                    match run_active_sync_cancellable(
                        &focus,
                        &fast_graph,
                        &fast_storage,
                        &fast_event_tx,
                        &mut last_active_sig,
                        false,
                        &mut focus_rx,
                        &mut fast_shutdown_rx,
                    )
                    .await
                    {
                        ActiveSyncRunOutcome::Completed(Ok(())) => {
                            last_active_sync_at = Some(Instant::now());
                            if active_err_backoff.record_success() {
                                let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                    busy: false,
                                    summary: "Synced".to_string(),
                                });
                            }
                        }
                        ActiveSyncRunOutcome::Completed(Err(err)) => {
                            handle_lane1_error(
                                &err,
                                &focus,
                                &fast_shared,
                                &mut restricted_convs,
                                &mut active_err_backoff,
                                &fast_event_tx,
                            );
                        }
                        ActiveSyncRunOutcome::FocusChanged => {
                            pending_focus_sync = true;
                            continue;
                        }
                        ActiveSyncRunOutcome::Shutdown => break,
                    }
                }
                active_tick.reset();
                continue;
            }

            tokio::select! {
                _ = fast_shutdown_rx.changed() => {
                    if *fast_shutdown_rx.borrow() {
                        break;
                    }
                }
                cmd = nudge_rx.recv() => {
                    match cmd {
                        None | Some(SyncCommand::Shutdown) => {
                            let _ = shutdown_tx.send(true);
                            break;
                        }
                        Some(cmd @ SyncCommand::RefreshConversations) => {
                            fast_shared.record_activity(Instant::now());
                            fast_shared.clear_auth_pause();
                            restricted_convs.clear();
                            let _ = bg_tx.send(cmd);
                        }
                        Some(SyncCommand::SyncActiveNow) => {
                            fast_shared.record_activity(Instant::now());
                            fast_shared.clear_auth_pause();
                            restricted_convs.clear();
                            active_err_backoff.reset_window();
                            let focus = focus_rx.borrow_and_update().clone();
                            match run_active_sync_cancellable(
                                &focus,
                                &fast_graph,
                                &fast_storage,
                                &fast_event_tx,
                                &mut last_active_sig,
                                false,
                                &mut focus_rx,
                                &mut fast_shutdown_rx,
                            )
                            .await
                            {
                                ActiveSyncRunOutcome::Completed(Ok(())) => {
                                    last_active_sync_at = Some(Instant::now());
                                    if active_err_backoff.record_success() {
                                        let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                            busy: false,
                                            summary: "Synced".to_string(),
                                        });
                                    }
                                }
                                ActiveSyncRunOutcome::Completed(Err(err)) => {
                                    handle_lane1_error(
                                        &err,
                                        &focus,
                                        &fast_shared,
                                        &mut restricted_convs,
                                        &mut active_err_backoff,
                                        &fast_event_tx,
                                    );
                                }
                                ActiveSyncRunOutcome::FocusChanged => {
                                    pending_focus_sync = true;
                                    continue;
                                }
                                ActiveSyncRunOutcome::Shutdown => break,
                            }
                            active_tick.reset();
                        }
                        Some(SyncCommand::SyncChannelHistory { team_id, channel_id }) => {
                            fast_shared.record_activity(Instant::now());
                            fast_shared.clear_auth_pause();
                            let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                busy: true,
                                summary: "Syncing channel history...".to_string(),
                            });
                            match sync_channel_history(&team_id, &channel_id, &fast_graph, &fast_storage, &fast_event_tx).await {
                                Ok(()) => {
                                    active_err_backoff.record_success();
                                    let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                        busy: false,
                                        summary: "Synced".to_string(),
                                    });
                                }
                                Err(err) => {
                                    let _ = fast_event_tx.send(CoreEvent::SyncError(format!("Failed to sync channel: {err}")));
                                    let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                        busy: false,
                                        summary: POLL_RETRY_SUMMARY.to_string(),
                                    });
                                }
                            }
                        }
                        Some(SyncCommand::NotifyUserActivity) => {
                            let now = Instant::now();
                            let (prev_idle, _) = fast_shared.idle_snapshot(now);
                            fast_shared.record_activity(now);
                            let returning_from_idle =
                                prev_idle >= Duration::from_secs(IDLE_THRESHOLD_SECS);
                            let throttled = last_active_sync_at
                                .is_some_and(|t| now.saturating_duration_since(t) < resume_min_gap);
                            if returning_from_idle
                                && !throttled
                                && !fast_shared.is_auth_paused(now)
                                && !active_err_backoff.is_backed_off(now)
                            {
                                let focus = focus_rx.borrow_and_update().clone();
                                let allowed = focus
                                    .target
                                    .as_ref()
                                    .is_some_and(|t| !restricted_convs.contains(&t.conv_key()));
                                if allowed {
                                    match run_active_sync_cancellable(
                                        &focus,
                                        &fast_graph,
                                        &fast_storage,
                                        &fast_event_tx,
                                        &mut last_active_sig,
                                        true,
                                        &mut focus_rx,
                                        &mut fast_shutdown_rx,
                                    )
                                    .await
                                    {
                                        ActiveSyncRunOutcome::Completed(Ok(())) => {
                                            last_active_sync_at = Some(Instant::now());
                                            if active_err_backoff.record_success() {
                                                let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                                    busy: false,
                                                    summary: "Synced".to_string(),
                                                });
                                            }
                                            active_tick.reset();
                                        }
                                        ActiveSyncRunOutcome::Completed(Err(err)) => {
                                            handle_lane1_error(
                                                &err,
                                                &focus,
                                                &fast_shared,
                                                &mut restricted_convs,
                                                &mut active_err_backoff,
                                                &fast_event_tx,
                                            );
                                            active_tick.reset();
                                        }
                                        ActiveSyncRunOutcome::FocusChanged => {
                                            pending_focus_sync = true;
                                            continue;
                                        }
                                        ActiveSyncRunOutcome::Shutdown => break,
                                    }
                                }
                            }
                        }
                        Some(SyncCommand::SetWindowFocus { focused }) => {
                            let now = Instant::now();
                            let (prev_idle, _) = fast_shared.idle_snapshot(now);
                            let next_state = if focused {
                                WindowFocusState::Focused
                            } else {
                                WindowFocusState::Unfocused
                            };
                            let prev_focus = fast_shared.set_window_focus(next_state);
                            if focused {
                                fast_shared.record_activity(now);
                                let should_resume_check = prev_focus == WindowFocusState::Unfocused
                                    || prev_idle >= Duration::from_secs(IDLE_THRESHOLD_SECS);
                                let throttled = last_active_sync_at.is_some_and(|t| {
                                    now.saturating_duration_since(t) < resume_min_gap
                                });
                                if should_resume_check
                                    && !throttled
                                    && !fast_shared.is_auth_paused(now)
                                    && !active_err_backoff.is_backed_off(now)
                                {
                                    let focus = focus_rx.borrow_and_update().clone();
                                    let allowed = focus
                                        .target
                                        .as_ref()
                                        .is_some_and(|t| !restricted_convs.contains(&t.conv_key()));
                                    if allowed {
                                        match run_active_sync_cancellable(
                                            &focus,
                                            &fast_graph,
                                            &fast_storage,
                                            &fast_event_tx,
                                            &mut last_active_sig,
                                            true,
                                            &mut focus_rx,
                                            &mut fast_shutdown_rx,
                                        )
                                        .await
                                        {
                                            ActiveSyncRunOutcome::Completed(Ok(())) => {
                                                last_active_sync_at = Some(Instant::now());
                                                if active_err_backoff.record_success() {
                                                    let _ = fast_event_tx.send(
                                                        CoreEvent::SyncStateChanged {
                                                            busy: false,
                                                            summary: "Synced".to_string(),
                                                        },
                                                    );
                                                }
                                                active_tick.reset();
                                            }
                                            ActiveSyncRunOutcome::Completed(Err(err)) => {
                                                handle_lane1_error(
                                                    &err,
                                                    &focus,
                                                    &fast_shared,
                                                    &mut restricted_convs,
                                                    &mut active_err_backoff,
                                                    &fast_event_tx,
                                                );
                                                active_tick.reset();
                                            }
                                            ActiveSyncRunOutcome::FocusChanged => {
                                                pending_focus_sync = true;
                                                continue;
                                            }
                                            ActiveSyncRunOutcome::Shutdown => break,
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                changed = focus_rx.changed() => {
                    if changed.is_err() {
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                    pending_focus_sync = true;
                }
                _ = active_tick.tick() => {
                    let now = Instant::now();
                    let (idle, seq) = fast_shared.idle_snapshot(now);
                    let mult = effective_active_idle_multiplier(fast_shared.window_focus(), idle);
                    if active_backoff.on_tick_with_multiplier(mult, seq)
                        && !fast_shared.is_auth_paused(now)
                        && !active_err_backoff.is_backed_off(now)
                    {
                        let focus = focus_rx.borrow_and_update().clone();
                        let allowed = focus
                            .target
                            .as_ref()
                            .is_some_and(|t| !restricted_convs.contains(&t.conv_key()));
                        if allowed {
                            match run_active_sync_cancellable(
                                &focus,
                                &fast_graph,
                                &fast_storage,
                                &fast_event_tx,
                                &mut last_active_sig,
                                true,
                                &mut focus_rx,
                                &mut fast_shutdown_rx,
                            )
                            .await
                            {
                                ActiveSyncRunOutcome::Completed(Ok(())) => {
                                    last_active_sync_at = Some(Instant::now());
                                    if active_err_backoff.record_success() {
                                        let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                            busy: false,
                                            summary: "Synced".to_string(),
                                        });
                                    }
                                }
                                ActiveSyncRunOutcome::Completed(Err(err)) => {
                                    handle_lane1_error(
                                        &err,
                                        &focus,
                                        &fast_shared,
                                        &mut restricted_convs,
                                        &mut active_err_backoff,
                                        &fast_event_tx,
                                    );
                                }
                                ActiveSyncRunOutcome::FocusChanged => {
                                    pending_focus_sync = true;
                                    continue;
                                }
                                ActiveSyncRunOutcome::Shutdown => break,
                            }
                        }
                    }
                }
            }
        }
    });

    // Lane 2: Background Poll Lane (chat preview diff + $batch, rotating channel delta, presence polling, bulk refreshes)
    let bg_shared = Arc::clone(&shared);
    let bg_focus_rx = focus_tx.subscribe();
    tokio::spawn(async move {
        let mut chat_tick = time::interval(Duration::from_millis(chat_ms));
        chat_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut channel_tick = time::interval(Duration::from_millis(channel_ms));
        channel_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut chat_backoff = PeriodicBackoffTracker::default();
        let mut channel_backoff = PeriodicBackoffTracker::default();
        let mut was_idle = false;
        let mut chat_cache: ChatPollCache = HashMap::new();
        let mut chat_cache_seeded = false;
        let mut chats_cursor_initialized = false;
        let mut rolling_chat_cursor: usize = 0;
        let mut channel_rotation = ChannelRotationState::default();
        let mut restricted_channels: HashSet<String> = HashSet::new();
        let mut presence_cache: PresencePollCache = HashMap::new();
        let mut presence_disabled_for_token: Option<String> = None;
        let mut chat_err_backoff = ErrorBackoffState::new(chat_ms);
        let mut channel_err_backoff = ErrorBackoffState::new(channel_ms);
        let mut presence_err_backoff = ErrorBackoffState::new(channel_ms);

        // Consume initial immediate ticks so startup isn't flooded simultaneously before focus/commands arrive.
        chat_tick.tick().await;
        channel_tick.tick().await;

        let mut shutdown_rx = shutdown_rx;
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        break;
                    }
                }
                cmd = bg_rx.recv() => {
                    match cmd {
                        None => break,
                        Some(SyncCommand::RefreshConversations) => {
                            bg_shared.clear_auth_pause();
                            presence_disabled_for_token = None;
                            restricted_channels.clear();
                            chat_err_backoff.reset_window();
                            channel_err_backoff.reset_window();
                            presence_err_backoff.reset_window();
                            let current_pins = tokio::task::spawn_blocking(|| {
                                AppConfig::load_from(&crate::config::config_file_path())
                                    .ok()
                                    .map(|cfg| cfg.pinned_convs.into_iter().collect::<HashSet<_>>())
                            })
                            .await
                            .ok()
                            .flatten();
                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                busy: true,
                                summary: "Syncing conversations...".to_string(),
                            });
                            match refresh_all_conversations_with_cache(
                                &graph,
                                &storage,
                                &event_tx,
                                &mut chat_cache,
                                &mut chat_cache_seeded,
                                &mut chats_cursor_initialized,
                                &mut rolling_chat_cursor,
                                current_pins.as_ref().unwrap_or(&config_pinned),
                                Some(&bg_focus_rx),
                                None,
                                Some(&mut presence_cache),
                            ).await {
                                Ok(()) => {
                                    chat_err_backoff.record_success();
                                    channel_err_backoff.record_success();
                                    presence_err_backoff.record_success();
                                    let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                        busy: false,
                                        summary: "Synced".to_string(),
                                    });
                                }
                                Err(err) => {
                                    let _ = event_tx.send(CoreEvent::SyncError(format!("Failed to sync conversations: {err}")));
                                    let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                        busy: false,
                                        summary: POLL_RETRY_SUMMARY.to_string(),
                                    });
                                }
                            }
                        }
                        Some(_) => {}
                    }
                }
                _ = chat_tick.tick() => {
                    let now = Instant::now();
                    let (idle, seq) = bg_shared.idle_snapshot(now);
                    maybe_shrink_on_idle_async(idle, &mut was_idle, &storage);
                    if chat_backoff.on_tick(idle, seq)
                        && !bg_shared.is_auth_paused(now)
                        && !chat_err_backoff.is_backed_off(now)
                    {
                        match poll_chats_tier2_with_rolling_cache(
                            &graph,
                            &storage,
                            &event_tx,
                            &mut chat_cache,
                            &mut chat_cache_seeded,
                            &mut chats_cursor_initialized,
                            &mut rolling_chat_cursor,
                        ).await {
                            Ok(()) => {
                                let was_degraded = chat_err_backoff.is_degraded()
                                    || channel_err_backoff.is_degraded()
                                    || presence_err_backoff.is_degraded();
                                chat_err_backoff.record_success();
                                let now_degraded = chat_err_backoff.is_degraded()
                                    || channel_err_backoff.is_degraded()
                                    || presence_err_backoff.is_degraded();
                                if was_degraded && !now_degraded {
                                    let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                        busy: false,
                                        summary: "Synced".to_string(),
                                    });
                                }
                            }
                            Err(err) => {
                                match classify_sync_error(&err) {
                                    SyncErrorKind::Unauthorized401 => {
                                        bg_shared.pause_auth(now, Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
                                    }
                                    SyncErrorKind::Forbidden403 => {
                                        // Quietly back off on forbidden /me/chats without toast spam.
                                        chat_err_backoff.record_transient_failure(now);
                                    }
                                    SyncErrorKind::Transient | SyncErrorKind::NotFound404 => {
                                        let was_degraded = chat_err_backoff.is_degraded()
                                            || channel_err_backoff.is_degraded()
                                            || presence_err_backoff.is_degraded();
                                        chat_err_backoff.record_transient_failure(now);
                                        let now_degraded = chat_err_backoff.is_degraded()
                                            || channel_err_backoff.is_degraded()
                                            || presence_err_backoff.is_degraded();
                                        if !was_degraded && now_degraded {
                                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                                busy: false,
                                                summary: POLL_RETRY_SUMMARY.to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                _ = channel_tick.tick() => {
                    let now = Instant::now();
                    let (idle, seq) = bg_shared.idle_snapshot(now);
                    maybe_shrink_on_idle_async(idle, &mut was_idle, &storage);
                    if channel_backoff.on_tick(idle, seq) && !bg_shared.is_auth_paused(now) {
                        let run_channels = !channel_err_backoff.is_backed_off(now);
                        let run_presence = idle < Duration::from_secs(IDLE_THRESHOLD_SECS)
                            && !presence_err_backoff.is_backed_off(now);

                        let active_key = bg_focus_rx
                            .borrow()
                            .target
                            .as_ref()
                            .map(ConversationTarget::conv_key);
                        let channel_fut = async {
                            if !run_channels {
                                return None;
                            }
                            let current_pins = tokio::task::spawn_blocking(|| {
                                AppConfig::load_from(&crate::config::config_file_path())
                                    .ok()
                                    .map(|cfg| cfg.pinned_convs.into_iter().collect::<HashSet<_>>())
                            })
                            .await
                            .ok()
                            .flatten();
                            Some(
                                poll_channels_tier3_rotating(
                                    &graph,
                                    &storage,
                                    &event_tx,
                                    Some(&bg_focus_rx),
                                    current_pins.as_ref().unwrap_or(&config_pinned),
                                    active_key.as_deref(),
                                    &mut channel_rotation,
                                    &mut restricted_channels,
                                )
                                .await,
                            )
                        };
                        let presence_fut = async {
                            if !run_presence {
                                return None;
                            }
                            let token_fp = current_token_fingerprint(&graph).await;
                            if presence_disabled_for_token.as_ref() == Some(&token_fp) {
                                return None;
                            }
                            let res = poll_presences_tier3_with_cache(
                                &graph,
                                &storage,
                                &event_tx,
                                &mut presence_cache,
                            )
                            .await;
                            Some((token_fp, res))
                        };

                        let (channel_res_opt, presence_res_opt) =
                            tokio::join!(channel_fut, presence_fut);

                        if let Some(channel_res) = channel_res_opt {
                            match channel_res {
                                Ok(()) => {
                                    let was_degraded = chat_err_backoff.is_degraded()
                                        || channel_err_backoff.is_degraded()
                                        || presence_err_backoff.is_degraded();
                                    channel_err_backoff.record_success();
                                    let now_degraded = chat_err_backoff.is_degraded()
                                        || channel_err_backoff.is_degraded()
                                        || presence_err_backoff.is_degraded();
                                    if was_degraded && !now_degraded {
                                        let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                            busy: false,
                                            summary: "Synced".to_string(),
                                        });
                                    }
                                }
                                Err(err) => match classify_sync_error(&err) {
                                    SyncErrorKind::Unauthorized401 => {
                                        bg_shared.pause_auth(
                                            now,
                                            Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS),
                                        );
                                    }
                                    SyncErrorKind::Forbidden403 => {}
                                    SyncErrorKind::Transient | SyncErrorKind::NotFound404 => {
                                        let was_degraded = chat_err_backoff.is_degraded()
                                            || channel_err_backoff.is_degraded()
                                            || presence_err_backoff.is_degraded();
                                        channel_err_backoff.record_transient_failure(now);
                                        let now_degraded = chat_err_backoff.is_degraded()
                                            || channel_err_backoff.is_degraded()
                                            || presence_err_backoff.is_degraded();
                                        if !was_degraded && now_degraded {
                                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                                busy: false,
                                                summary: POLL_RETRY_SUMMARY.to_string(),
                                            });
                                        }
                                    }
                                },
                            }
                        }

                        if let Some((token_fp, presence_res)) = presence_res_opt {
                            presence_disabled_for_token = None;
                            match presence_res {
                                Ok(()) => {
                                    let was_degraded = chat_err_backoff.is_degraded()
                                        || channel_err_backoff.is_degraded()
                                        || presence_err_backoff.is_degraded();
                                    presence_err_backoff.record_success();
                                    let now_degraded = chat_err_backoff.is_degraded()
                                        || channel_err_backoff.is_degraded()
                                        || presence_err_backoff.is_degraded();
                                    if was_degraded && !now_degraded {
                                        let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                            busy: false,
                                            summary: "Synced".to_string(),
                                        });
                                    }
                                }
                                Err(err) => match classify_sync_error(&err) {
                                    SyncErrorKind::Forbidden403 => {
                                        presence_disabled_for_token = Some(token_fp);
                                    }
                                    SyncErrorKind::Unauthorized401 => {
                                        bg_shared.pause_auth(
                                            now,
                                            Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS),
                                        );
                                    }
                                    SyncErrorKind::Transient | SyncErrorKind::NotFound404 => {
                                        let was_degraded = chat_err_backoff.is_degraded()
                                            || channel_err_backoff.is_degraded()
                                            || presence_err_backoff.is_degraded();
                                        presence_err_backoff.record_transient_failure(now);
                                        let now_degraded = chat_err_backoff.is_degraded()
                                            || channel_err_backoff.is_degraded()
                                            || presence_err_backoff.is_degraded();
                                        if !was_degraded && now_degraded {
                                            let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                                busy: false,
                                                summary: POLL_RETRY_SUMMARY.to_string(),
                                            });
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
            }
        }
    });

    SyncController {
        focus_tx,
        nudge_tx,
        shared,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn maybe_shrink_on_idle(idle_duration: Duration, was_idle: &mut bool, storage: &Storage) {
    if idle_duration >= Duration::from_secs(IDLE_THRESHOLD_SECS) {
        if !*was_idle {
            *was_idle = true;
            storage.shrink_memory();
        }
    } else {
        *was_idle = false;
    }
}

fn maybe_shrink_on_idle_async(idle_duration: Duration, was_idle: &mut bool, storage: &Storage) {
    if idle_duration >= Duration::from_secs(IDLE_THRESHOLD_SECS) {
        if !*was_idle {
            *was_idle = true;
            let st = storage.clone();
            tokio::task::spawn_blocking(move || st.shrink_memory());
        }
    } else {
        *was_idle = false;
    }
}

pub(crate) fn save_initial_history_cursor_if_absent(
    storage: &Storage,
    conv_key: &str,
    msgs: &[MessageItem],
    next_link: Option<String>,
) -> Result<()> {
    let scope = SyncCursor::history_scope(conv_key);
    if storage.get_sync_cursor(&scope)?.is_none() {
        let oldest_msg_ts = if next_link.is_none() {
            Some(SyncCursor::HISTORY_EOF.to_string())
        } else {
            msgs.iter()
                .map(|m| m.created_at.as_str())
                .min()
                .map(str::to_string)
        };
        storage.save_sync_cursor(&SyncCursor {
            scope_key: scope,
            delta_link: next_link,
            oldest_msg_ts,
            synced_at: chrono::Utc::now().to_rfc3339(),
        })?;
    }
    Ok(())
}

/// Records the chat-list cursor from a fresh page-1 `/me/chats` response.
///
/// A stored link to a later page is kept, so page 1 never rewinds pagination. A stored
/// `HISTORY_EOF` or a cleared (expired) link is replaced: page 1 shows what Graph has now,
/// and an EOF saved when the list was shorter would otherwise hide older chats for good.
fn save_initial_chats_list_cursor(storage: &Storage, next_link: Option<String>) -> Result<()> {
    let next_link = next_link
        .map(|link| link.trim().to_string())
        .filter(|link| !link.is_empty());
    let keep_existing = storage
        .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)?
        .is_some_and(|cursor| {
            cursor
                .delta_link
                .as_deref()
                .is_some_and(|link| !link.trim().is_empty())
        });
    if keep_existing {
        return Ok(());
    }
    let oldest_msg_ts = if next_link.is_none() {
        Some(SyncCursor::HISTORY_EOF.to_string())
    } else {
        None
    };
    storage.save_sync_cursor(&SyncCursor {
        scope_key: SyncCursor::CHATS_LIST_SCOPE.to_string(),
        delta_link: next_link,
        oldest_msg_ts,
        synced_at: chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg_attr(not(test), allow(dead_code))]
async fn sync_active_focus(
    focus: &FocusTarget,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let mut sigs = HashMap::new();
    sync_active_focus_with_state(focus, graph, storage, event_tx, &mut sigs, false).await
}

fn is_focus_still_current(
    live_focus_rx: Option<&watch::Receiver<FocusTarget>>,
    expected: &FocusTarget,
) -> bool {
    match live_focus_rx {
        Some(rx) => !rx.has_changed().unwrap_or(false) && *rx.borrow() == *expected,
        None => true,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
async fn sync_active_focus_with_state(
    focus: &FocusTarget,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    last_active_sig: &mut HashMap<String, u64>,
    is_periodic: bool,
) -> Result<()> {
    sync_active_focus_inner(
        focus,
        graph,
        storage,
        event_tx,
        last_active_sig,
        is_periodic,
        None,
    )
    .await
}

async fn sync_active_focus_inner(
    focus: &FocusTarget,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    last_active_sig: &mut HashMap<String, u64>,
    is_periodic: bool,
    live_focus_rx: Option<&watch::Receiver<FocusTarget>>,
) -> Result<()> {
    let Some(target) = &focus.target else {
        return Ok(());
    };
    let conv_key = target.conv_key();

    match target {
        ConversationTarget::Chat { .. } => {
            let (messages, next_link) = graph.list_messages_page(target, 50, None).await?;
            if !is_focus_still_current(live_focus_rx, focus) {
                return Ok(());
            }
            let sig = compute_messages_signature(&messages);
            let prev_sig = last_active_sig.get(&conv_key).copied();
            if is_periodic && prev_sig == Some(sig) {
                return Ok(());
            }

            let key_for_db = conv_key.clone();
            let (changed_ids, changed_messages) = run_storage(storage, move |st| {
                let outcome = st.upsert_messages_detailed(&messages)?;
                save_initial_history_cursor_if_absent(st, &key_for_db, &messages, next_link)?;
                let filtered = filter_changed_messages(messages, &outcome.changed_ids);
                Ok((outcome.changed_ids, filtered))
            })
            .await?;
            if !is_focus_still_current(live_focus_rx, focus) {
                return Ok(());
            }
            last_active_sig.insert(conv_key.clone(), sig);

            if !is_periodic || prev_sig.is_none() || !changed_ids.is_empty() {
                let _ = event_tx.send(CoreEvent::MessagesUpdated {
                    conv_key,
                    parent_msg_id: None,
                    messages: changed_messages,
                    notify_candidate: None,
                    changed_msg_ids: Some(changed_ids),
                });
            }
        }
        ConversationTarget::Channel {
            team_id,
            channel_id,
        } => {
            let key_for_cursor = conv_key.clone();
            let cursor =
                run_storage(storage, move |st| st.get_sync_cursor(&key_for_cursor)).await?;
            let requested_at = chrono::Utc::now();
            let delta_fut = graph.sync_channel_messages_delta(
                team_id,
                channel_id,
                cursor.as_ref().and_then(|c| c.delta_link.as_deref()),
            );
            let replies_fut = async {
                match &focus.thread_root_id {
                    Some(root_id) => Some((
                        root_id.clone(),
                        graph
                            .list_thread_replies(team_id, channel_id, root_id, 200)
                            .await,
                    )),
                    None => None,
                }
            };
            let (delta_res, replies_opt) = tokio::join!(delta_fut, replies_fut);
            if !is_focus_still_current(live_focus_rx, focus) {
                return Ok(());
            }

            match delta_res {
                Ok((messages, new_delta_link)) => {
                    let oldest_ts = messages
                        .iter()
                        .map(|m| m.created_at.as_str())
                        .min()
                        .map(str::to_string)
                        .or_else(|| cursor.as_ref().and_then(|c| c.oldest_msg_ts.clone()));
                    let effective_delta_link = new_delta_link
                        .or_else(|| cursor.as_ref().and_then(|c| c.delta_link.clone()));
                    let new_cursor = SyncCursor {
                        scope_key: conv_key.clone(),
                        delta_link: effective_delta_link,
                        oldest_msg_ts: oldest_ts,
                        synced_at: requested_at.to_rfc3339(),
                    };

                    let sig = compute_messages_signature(&messages);
                    let prev_sig = last_active_sig.get(&conv_key).copied();

                    let msgs_for_db = messages.clone();
                    let outcome = run_storage(storage, move |st| {
                        st.apply_channel_delta(
                            cursor.as_ref(),
                            &new_cursor,
                            &msgs_for_db,
                            &HashSet::new(),
                        )
                    })
                    .await?;
                    if !is_focus_still_current(live_focus_rx, focus) {
                        return Ok(());
                    }

                    if let Some(outcome) = outcome.filter(|_| !messages.is_empty()) {
                        last_active_sig.insert(conv_key.clone(), sig);
                        if !is_periodic || prev_sig.is_none() || !outcome.changed_ids.is_empty() {
                            let changed_ids = if outcome.changed_ids.is_empty() {
                                None
                            } else {
                                Some(outcome.changed_ids)
                            };
                            let _ = event_tx.send(CoreEvent::MessagesUpdated {
                                conv_key: conv_key.clone(),
                                parent_msg_id: None,
                                messages,
                                notify_candidate: None,
                                changed_msg_ids: changed_ids,
                            });
                        }
                    }
                }
                Err(err) => {
                    if !is_expired_delta_error(&err) {
                        return Err(err);
                    }
                    let Some(cleared_cursor) =
                        clear_channel_delta_cursor(storage, &conv_key, cursor).await?
                    else {
                        return Ok(());
                    };

                    let (messages, next_link) = graph.list_messages_page(target, 50, None).await?;
                    if !is_focus_still_current(live_focus_rx, focus) {
                        return Ok(());
                    }
                    let sig = compute_messages_signature(&messages);
                    let prev_sig = last_active_sig.get(&conv_key).copied();
                    if !(is_periodic && prev_sig == Some(sig)) {
                        let msgs_for_db = messages.clone();
                        let key_for_db = conv_key.clone();
                        let outcome = run_storage(storage, move |st| {
                            let outcome = st.apply_channel_delta(
                                Some(&cleared_cursor),
                                &cleared_cursor,
                                &msgs_for_db,
                                &HashSet::new(),
                            )?;
                            if outcome.is_some() {
                                save_initial_history_cursor_if_absent(
                                    st,
                                    &key_for_db,
                                    &msgs_for_db,
                                    next_link,
                                )?;
                            }
                            Ok(outcome)
                        })
                        .await?;
                        if !is_focus_still_current(live_focus_rx, focus) {
                            return Ok(());
                        }
                        let Some(outcome) = outcome else {
                            return Ok(());
                        };
                        last_active_sig.insert(conv_key.clone(), sig);

                        if !is_periodic || prev_sig.is_none() || !outcome.changed_ids.is_empty() {
                            let _ = event_tx.send(CoreEvent::MessagesUpdated {
                                conv_key: conv_key.clone(),
                                parent_msg_id: None,
                                messages,
                                notify_candidate: None,
                                changed_msg_ids: None,
                            });
                        }
                    }
                }
            }

            if let Some((root_id, replies_res)) = replies_opt {
                if !is_focus_still_current(live_focus_rx, focus) {
                    return Ok(());
                }
                let replies = replies_res?;
                let thread_sig_key = format!("{conv_key}|thread:{root_id}");
                let sig = compute_messages_signature(&replies);
                let prev_sig = last_active_sig.get(&thread_sig_key).copied();
                if !(is_periodic && prev_sig == Some(sig)) {
                    let replies_for_db = replies.clone();
                    let changed_rows =
                        run_storage(storage, move |st| st.upsert_messages(&replies_for_db))
                            .await?;
                    if !is_focus_still_current(live_focus_rx, focus) {
                        return Ok(());
                    }
                    last_active_sig.insert(thread_sig_key, sig);

                    if !is_periodic || prev_sig.is_none() || changed_rows > 0 {
                        let _ = event_tx.send(CoreEvent::MessagesUpdated {
                            conv_key,
                            parent_msg_id: Some(root_id),
                            messages: replies,
                            notify_candidate: None,
                            changed_msg_ids: None,
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
async fn poll_chats_tier2_with_cache(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    chat_cache: &mut ChatPollCache,
    chat_cache_seeded: &mut bool,
    chats_cursor_initialized: &mut bool,
) -> Result<()> {
    let mut rolling_cursor = 0usize;
    poll_chats_tier2_with_rolling_cache(
        graph,
        storage,
        event_tx,
        chat_cache,
        chat_cache_seeded,
        chats_cursor_initialized,
        &mut rolling_cursor,
    )
    .await
}

async fn poll_chats_tier2_with_rolling_cache(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    chat_cache: &mut ChatPollCache,
    chat_cache_seeded: &mut bool,
    chats_cursor_initialized: &mut bool,
    rolling_cursor: &mut usize,
) -> Result<()> {
    // Seed known conversations at startup, and refresh newly-discovered DB chats at the start
    // of each rotation cycle so older chats loaded beyond page 1 also enter rotation.
    if !*chat_cache_seeded || *rolling_cursor == 0 {
        let existing = run_storage(storage, |st| st.list_conversations()).await?;
        for conv in existing {
            if !conv.kind.is_channel() {
                chat_cache.entry(conv.conv_key).or_insert(ChatPollCacheEntry {
                    last_msg_id: conv.last_msg_id,
                    last_updated_at: conv.last_updated_at,
                    last_msg_preview: conv.last_msg_preview,
                    title: conv.title,
                    unread_count: conv.unread_count,
                });
            }
        }
        *chat_cache_seeded = true;
    }

    let chats_page = graph.list_chats_page_detailed(50, None).await?;
    let chats = chats_page.chats;
    let next_link = chats_page.next_link;
    let explicit_null_topic_keys = chats_page.explicit_null_topic_keys;

    let mut dirty_ids = Vec::new();
    let mut dirty_set = HashSet::new();
    let mut clean_pool = Vec::new();
    let mut clean_seen = HashSet::new();
    let mut changed_chats = Vec::new();
    for chat in &chats {
        let next_entry = ChatPollCacheEntry {
            last_msg_id: chat.last_msg_id.clone(),
            last_updated_at: chat.last_updated_at.clone(),
            last_msg_preview: chat.last_msg_preview.clone(),
            title: chat.title.clone(),
            unread_count: chat.unread_count,
        };
        let cached = chat_cache.get(&chat.conv_key);
        let changed = cached != Some(&next_entry);
        if changed {
            changed_chats.push(chat.clone());
        }

        if let ConversationTarget::Chat { chat_id } = &chat.target {
            let is_dirty = match cached {
                None => chat.last_msg_id.is_some() || chat.last_updated_at.is_some(),
                Some(entry) => {
                    (chat.last_msg_id.is_some() && chat.last_msg_id != entry.last_msg_id)
                        || (chat.last_updated_at.is_some()
                            && chat.last_updated_at != entry.last_updated_at)
                        || (chat.last_msg_preview != entry.last_msg_preview)
                }
            };
            if is_dirty {
                if dirty_set.insert(chat_id.clone()) {
                    dirty_ids.push(chat_id.clone());
                }
            } else if clean_seen.insert(chat_id.clone()) {
                clean_pool.push(chat_id.clone());
            }
        }
    }

    // Include known cached chats beyond page 1 of `/me/chats?$top=50` in deterministic order
    // so older cached inactive conversations also get bounded rotating turns without extra pages.
    let mut beyond_page1_ids: Vec<String> = chat_cache
        .keys()
        .filter_map(|conv_key| conv_key.strip_prefix("chat:").map(str::to_string))
        .filter(|cid| !dirty_set.contains(cid) && !clean_seen.contains(cid))
        .collect();
    beyond_page1_ids.sort();
    clean_pool.extend(beyond_page1_ids);

    // Keep each tick strictly bounded to one `$batch` request (<= MAX_TIER2_BATCH_SUBREQS = 20)
    // while always reserving rotating slots so 20+ dirty chats cannot starve inactive rotation.
    let rolling_slots = if clean_pool.is_empty() {
        0
    } else if dirty_ids.len() + ROLLING_CHAT_REVALIDATE_BATCH <= MAX_TIER2_BATCH_SUBREQS {
        ROLLING_CHAT_REVALIDATE_BATCH.min(clean_pool.len())
    } else {
        MIN_ROLLING_CHAT_SLOTS.min(clean_pool.len())
    };
    let dirty_budget = MAX_TIER2_BATCH_SUBREQS.saturating_sub(rolling_slots);

    let mut fetch_ids = Vec::with_capacity(MAX_TIER2_BATCH_SUBREQS);
    let mut deferred_dirty_keys: HashSet<String> = HashSet::new();
    for (idx, cid) in dirty_ids.iter().enumerate() {
        if idx < dirty_budget {
            fetch_ids.push(cid.clone());
        } else {
            deferred_dirty_keys.insert(
                ConversationTarget::Chat {
                    chat_id: cid.clone(),
                }
                .conv_key(),
            );
        }
    }
    if rolling_slots > 0 {
        let start = *rolling_cursor % clean_pool.len();
        for offset in 0..rolling_slots {
            fetch_ids.push(clean_pool[(start + offset) % clean_pool.len()].clone());
        }
        *rolling_cursor = (start + rolling_slots) % clean_pool.len();
    }

    let chat_by_id: HashMap<String, &ConversationSummary> = chats
        .iter()
        .filter_map(|c| match &c.target {
            ConversationTarget::Chat { chat_id } => Some((chat_id.clone(), c)),
            _ => None,
        })
        .collect();

    if !changed_chats.is_empty() || !*chats_cursor_initialized {
        let to_upsert = changed_chats.clone();
        let save_cursor = !*chats_cursor_initialized;
        run_storage(storage, move |st| {
            if !to_upsert.is_empty() {
                st.upsert_conversations_with_null_topics(&to_upsert, &explicit_null_topic_keys)?;
            }
            if save_cursor {
                save_initial_chats_list_cursor(st, next_link)?;
            }
            Ok(())
        })
        .await?;
        *chats_cursor_initialized = true;
    }

    let mut any_preview_updated = false;
    // Chats whose `$batch` sub-response was throttled, failed or unreadable this tick.
    let mut failed_keys: HashSet<String> = HashSet::new();
    if !fetch_ids.is_empty() {
        let fetched = graph.batch_fetch_chat_messages(&fetch_ids, 20).await?;
        let mut ready_meta: Vec<(String, String)> = Vec::new();
        let mut ready_batches: Vec<Vec<MessageItem>> = Vec::new();
        for (chat_id, messages) in fetched {
            let conv_key = ConversationTarget::Chat {
                chat_id: chat_id.clone(),
            }
            .conv_key();
            let Some(messages) = messages else {
                failed_keys.insert(conv_key);
                continue;
            };
            let conv_title = chat_by_id
                .get(&chat_id)
                .map(|c| c.title.clone())
                .or_else(|| chat_cache.get(&conv_key).map(|e| e.title.clone()))
                .unwrap_or_else(|| conv_key.clone());
            ready_meta.push((conv_key, conv_title));
            ready_batches.push(messages);
        }

        if !ready_batches.is_empty() {
            let batch_results = run_storage(storage, move |st| {
                let outcomes = st.upsert_message_batches_detailed(&ready_batches)?;
                let mut results = Vec::with_capacity(outcomes.len());
                for (messages, outcome) in ready_batches.into_iter().zip(outcomes) {
                    let inserted = filter_changed_messages_ref(&messages, &outcome.inserted_ids);
                    let changed = filter_changed_messages(messages, &outcome.changed_ids);
                    results.push((outcome, changed, inserted));
                }
                Ok(results)
            })
            .await?;

            for ((conv_key, conv_title), (outcome, changed_messages, newly_inserted_messages)) in
                ready_meta.into_iter().zip(batch_results)
            {
                if !outcome.preview_updated_convs.is_empty() {
                    any_preview_updated = true;
                }
                if !outcome.changed_ids.is_empty() {
                    let notify_candidate = build_notification_candidate(
                        &conv_key,
                        &conv_title,
                        &newly_inserted_messages,
                    );
                    let _ = event_tx.send(CoreEvent::MessagesUpdated {
                        conv_key,
                        parent_msg_id: None,
                        messages: changed_messages,
                        notify_candidate,
                        changed_msg_ids: Some(outcome.changed_ids),
                    });
                }
            }
        }
    }

    for chat in &changed_chats {
        // Keep deferred dirty chats marked stale in `chat_cache` so the next tick fetches them;
        // the same goes for a chat whose messages could not be fetched this tick.
        if deferred_dirty_keys.contains(&chat.conv_key) {
            continue;
        }
        if failed_keys.contains(&chat.conv_key) {
            // A chat seen for the first time has no entry yet. Leave a blank one, so the
            // start-of-rotation seeding from SQLite (which already holds the new summary)
            // cannot record it as clean before its messages were fetched.
            chat_cache
                .entry(chat.conv_key.clone())
                .or_insert_with(|| ChatPollCacheEntry {
                    last_msg_id: None,
                    last_updated_at: None,
                    last_msg_preview: None,
                    title: chat.title.clone(),
                    unread_count: chat.unread_count,
                });
            continue;
        }
        chat_cache.insert(
            chat.conv_key.clone(),
            ChatPollCacheEntry {
                last_msg_id: chat.last_msg_id.clone(),
                last_updated_at: chat.last_updated_at.clone(),
                last_msg_preview: chat.last_msg_preview.clone(),
                title: chat.title.clone(),
                unread_count: chat.unread_count,
            },
        );
    }

    if !changed_chats.is_empty() || any_preview_updated {
        let all_convs = run_storage(storage, |st| st.list_conversations()).await?;
        if any_preview_updated {
            for conv in &all_convs {
                if let Some(entry) = chat_cache.get_mut(&conv.conv_key) {
                    entry.last_msg_preview = conv.last_msg_preview.clone();
                }
            }
        }
        let _ = event_tx.send(CoreEvent::ConversationsUpdated(all_convs));
    }

    Ok(())
}

async fn poll_presences_tier3_with_cache(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    presence_cache: &mut PresencePollCache,
) -> Result<()> {
    let convs = run_storage(storage, |st| st.list_conversations()).await?;
    let mut seen = HashSet::new();
    let mut user_ids = Vec::new();
    for c in convs {
        if c.kind == ConversationKind::OneOnOne {
            if let Some(uid) = c.peer_user_id {
                if seen.insert(uid.clone()) {
                    user_ids.push(uid);
                    if user_ids.len() >= MAX_POLLED_PRESENCE_PEERS {
                        break;
                    }
                }
            }
        }
    }
    if user_ids.is_empty() {
        return Ok(());
    }

    let presences = graph.get_presences_by_user_ids(&user_ids).await?;
    let changed: Vec<PresenceStatus> = presences
        .into_iter()
        .filter(|p| {
            presence_cache
                .get(&p.user_id)
                .is_none_or(|(avail, act)| avail != &p.availability || act != &p.activity)
        })
        .collect();

    if !changed.is_empty() {
        let to_upsert = changed.clone();
        run_storage(storage, move |st| st.upsert_presences(&to_upsert)).await?;
        for p in &changed {
            presence_cache.insert(
                p.user_id.clone(),
                (p.availability.clone(), p.activity.clone()),
            );
        }
        let _ = event_tx.send(CoreEvent::PresenceUpdated(changed));
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChannelPollCandidate {
    pub team_id: String,
    pub channel_id: String,
    pub conv_key: String,
    pub conv_title: String,
    pub pinned: bool,
    pub last_updated_at: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct ChannelRotationState {
    polled_in_round: HashSet<String>,
}

impl ChannelRotationState {
    pub(crate) fn take_next(
        &mut self,
        candidates: &[ChannelPollCandidate],
        already_picked_this_tick: &HashSet<String>,
    ) -> Option<ChannelPollCandidate> {
        if candidates.is_empty() {
            return None;
        }
        if let Some(cand) = candidates.iter().find(|c| {
            !self.polled_in_round.contains(&c.conv_key)
                && !already_picked_this_tick.contains(&c.conv_key)
        }) {
            return Some(cand.clone());
        }
        if candidates.iter().all(|c| {
            self.polled_in_round.contains(&c.conv_key)
                || already_picked_this_tick.contains(&c.conv_key)
        }) {
            self.polled_in_round.clear();
            return candidates
                .iter()
                .find(|c| !already_picked_this_tick.contains(&c.conv_key))
                .cloned();
        }
        None
    }

    pub(crate) fn mark_polled(&mut self, conv_key: &str) {
        self.polled_in_round.insert(conv_key.to_string());
    }

    pub(crate) fn unmark_polled(&mut self, conv_key: &str) {
        self.polled_in_round.remove(conv_key);
    }

    #[cfg(test)]
    pub(crate) fn select_next_batch(
        &mut self,
        candidates: &[ChannelPollCandidate],
        max_batch: usize,
    ) -> Vec<ChannelPollCandidate> {
        let mut picked = Vec::with_capacity(max_batch.min(candidates.len()));
        let mut picked_this_tick = HashSet::with_capacity(max_batch.min(candidates.len()));
        for _ in 0..max_batch {
            let Some(cand) = self.take_next(candidates, &picked_this_tick) else {
                break;
            };
            self.mark_polled(&cand.conv_key);
            picked_this_tick.insert(cand.conv_key.clone());
            picked.push(cand);
        }
        picked
    }
}

pub(crate) fn build_channel_poll_candidates(
    teams: &[TeamSummary],
    conversations: &[ConversationSummary],
    config_pinned: &HashSet<String>,
    active_conv_key: Option<&str>,
    restricted_channels: &HashSet<String>,
) -> Vec<ChannelPollCandidate> {
    let mut conv_meta: HashMap<&str, (bool, Option<&str>)> =
        HashMap::with_capacity(conversations.len());
    for c in conversations {
        if c.kind == ConversationKind::Channel || c.conv_key.starts_with("chan:") {
            conv_meta.insert(
                c.conv_key.as_str(),
                (c.pinned, c.last_updated_at.as_deref()),
            );
        }
    }

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for team in teams {
        for ch in &team.channels {
            let conv_key = ch.conv_key();
            if active_conv_key == Some(conv_key.as_str())
                || restricted_channels.contains(&conv_key)
                || !seen.insert(conv_key.clone())
            {
                continue;
            }
            let (db_pinned, last_updated_at) = conv_meta
                .get(conv_key.as_str())
                .copied()
                .unwrap_or((false, None));
            let pinned = db_pinned || config_pinned.contains(&conv_key);
            candidates.push(ChannelPollCandidate {
                team_id: ch.team_id.clone(),
                channel_id: ch.id.clone(),
                conv_title: format!("{} › #{}", team.display_name, ch.display_name),
                conv_key,
                pinned,
                last_updated_at: last_updated_at.map(str::to_string),
            });
        }
    }

    candidates.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then_with(|| {
                b.last_updated_at
                    .as_deref()
                    .unwrap_or("")
                    .cmp(a.last_updated_at.as_deref().unwrap_or(""))
            })
            .then_with(|| a.conv_title.cmp(&b.conv_title))
            .then_with(|| a.conv_key.cmp(&b.conv_key))
    });
    candidates
}

#[derive(Debug, Default)]
struct ChannelDeltaOutcome {
    any_summary_changed: bool,
}

#[allow(clippy::too_many_arguments)]
async fn poll_channels_tier3_rotating(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    live_focus_rx: Option<&watch::Receiver<FocusTarget>>,
    config_pinned: &HashSet<String>,
    active_conv_key: Option<&str>,
    rotation: &mut ChannelRotationState,
    restricted_channels: &mut HashSet<String>,
) -> Result<()> {
    let (teams, convs) = run_storage(storage, |st| {
        Ok((st.list_teams()?, st.list_conversations()?))
    })
    .await?;
    let candidates = build_channel_poll_candidates(
        &teams,
        &convs,
        config_pinned,
        active_conv_key,
        restricted_channels,
    );
    if candidates.is_empty() {
        return Ok(());
    }

    let mut picked_this_tick: HashSet<String> = HashSet::with_capacity(BACKGROUND_CHANNEL_BATCH_SIZE);
    let mut batch: Vec<ChannelPollCandidate> = Vec::with_capacity(BACKGROUND_CHANNEL_BATCH_SIZE);

    for _ in 0..BACKGROUND_CHANNEL_BATCH_SIZE {
        let Some(cand) = rotation.take_next(&candidates, &picked_this_tick) else {
            break;
        };
        let now_active = live_focus_rx.is_some_and(|rx| {
            rx.borrow()
                .target
                .as_ref()
                .is_some_and(|t| t.conv_key() == cand.conv_key)
        });
        picked_this_tick.insert(cand.conv_key.clone());
        if now_active {
            continue;
        }

        rotation.mark_polled(&cand.conv_key);
        batch.push(cand);
    }

    if batch.is_empty() {
        return Ok(());
    }

    let mut any_summary_changed = false;
    let mut first_auth_err: Option<anyhow::Error> = None;
    let mut first_transient_err: Option<anyhow::Error> = None;

    let mut stream = futures_util::stream::iter(batch)
        .map(|cand| {
            let graph = graph.clone();
            let storage = storage.clone();
            let event_tx = event_tx.clone();
            let live_focus_rx = live_focus_rx.cloned();
            async move {
                let res = refresh_channel_delta_once(
                    &graph,
                    &storage,
                    &event_tx,
                    &cand.team_id,
                    &cand.channel_id,
                    &cand.conv_key,
                    &cand.conv_title,
                    true,
                    live_focus_rx.as_ref(),
                )
                .await;
                (cand.conv_key, res)
            }
        })
        .buffer_unordered(BACKGROUND_CHANNEL_BATCH_SIZE);

    while let Some((conv_key, res)) = stream.next().await {
        match res {
            Ok(outcome) => {
                any_summary_changed |= outcome.any_summary_changed;
            }
            Err(err) => match classify_sync_error(&err) {
                // A channel we may not read, or one that no longer exists (deleted, or its
                // team was left): drop it from the rotation until the next full teams refresh
                // and keep polling the rest of the batch.
                SyncErrorKind::Forbidden403 | SyncErrorKind::NotFound404 => {
                    restricted_channels.insert(conv_key);
                }
                SyncErrorKind::Unauthorized401 => {
                    rotation.unmark_polled(&conv_key);
                    if first_auth_err.is_none() {
                        first_auth_err = Some(err);
                    }
                }
                SyncErrorKind::Transient => {
                    rotation.unmark_polled(&conv_key);
                    if first_transient_err.is_none() {
                        first_transient_err = Some(err);
                    }
                }
            },
        }
    }

    if any_summary_changed {
        let all_convs = run_storage(storage, |st| st.list_conversations()).await?;
        let _ = event_tx.send(CoreEvent::ConversationsUpdated(all_convs));
    }

    if let Some(err) = first_auth_err.or(first_transient_err) {
        return Err(err);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn refresh_all_conversations_with_cache(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    chat_cache: &mut ChatPollCache,
    chat_cache_seeded: &mut bool,
    chats_cursor_initialized: &mut bool,
    rolling_cursor: &mut usize,
    config_pinned: &HashSet<String>,
    live_focus_rx: Option<&watch::Receiver<FocusTarget>>,
    fallback_active_conv_key: Option<&str>,
    presence_cache: Option<&mut PresencePollCache>,
) -> Result<()> {
    let (chats_res, teams_res) = tokio::join!(
        poll_chats_tier2_with_rolling_cache(
            graph,
            storage,
            event_tx,
            chat_cache,
            chat_cache_seeded,
            chats_cursor_initialized,
            rolling_cursor,
        ),
        graph.list_joined_teams_with_channels(),
    );
    let mut first_err: Option<anyhow::Error> = chats_res.err();

    let presence_fut = async {
        if let Some(p_cache) = presence_cache {
            let _ = poll_presences_tier3_with_cache(graph, storage, event_tx, p_cache).await;
        }
    };

    let teams_fut = async {
        match teams_res {
            Ok(fetched) => {
                let teams =
                    run_storage(storage, move |st| st.upsert_joined_teams(&fetched)).await?;
                let _ = event_tx.send(CoreEvent::TeamsUpdated(teams.clone()));
                let existing_convs = run_storage(storage, |st| st.list_conversations()).await?;
                let empty_restricted = HashSet::new();
                let mut candidates = build_channel_poll_candidates(
                    &teams,
                    &existing_convs,
                    config_pinned,
                    None,
                    &empty_restricted,
                );
                // Ensure the currently focused channel is always prioritized inside the refresh budget.
                let initial_live_active = live_focus_rx
                    .and_then(|rx| rx.borrow().target.as_ref().map(ConversationTarget::conv_key))
                    .or_else(|| fallback_active_conv_key.map(str::to_string));
                if let Some(ref active_key) = initial_live_active {
                    if let Some(pos) = candidates.iter().position(|c| &c.conv_key == active_key) {
                        let active_cand = candidates.remove(pos);
                        candidates.insert(0, active_cand);
                    }
                }

                // Explicit refresh reconciles up to MAX_REFRESH_CHANNELS via delta with bounded
                // concurrency so new posts, @me mentions, deletions, and unread counts surface
                // quickly without a 20-RTT serial waterfall.
                const REFRESH_CHANNEL_CONCURRENCY: usize = 5;
                let fallback_active = fallback_active_conv_key.map(str::to_string);
                futures_util::stream::iter(candidates.into_iter().take(MAX_REFRESH_CHANNELS))
                    .map(|cand| {
                        let graph = graph.clone();
                        let storage = storage.clone();
                        let event_tx = event_tx.clone();
                        let live_focus_rx = live_focus_rx.cloned();
                        let fallback_active = fallback_active.clone();
                        async move {
                            let live_active = live_focus_rx
                                .as_ref()
                                .and_then(|rx| {
                                    rx.borrow().target.as_ref().map(ConversationTarget::conv_key)
                                })
                                .or(fallback_active);
                            let is_active = live_active.as_deref() == Some(cand.conv_key.as_str());
                            let _ = refresh_channel_delta_once(
                                &graph,
                                &storage,
                                &event_tx,
                                &cand.team_id,
                                &cand.channel_id,
                                &cand.conv_key,
                                &cand.conv_title,
                                !is_active,
                                live_focus_rx.as_ref(),
                            )
                            .await;
                        }
                    })
                    .buffer_unordered(REFRESH_CHANNEL_CONCURRENCY)
                    .for_each(|()| async {})
                    .await;

                let all_convs = run_storage(storage, |st| st.list_conversations()).await?;
                let _ = event_tx.send(CoreEvent::ConversationsUpdated(all_convs));
                Ok(None)
            }
            Err(err) => Ok(Some(err)),
        }
    };

    let (teams_stage_res, ()) = tokio::join!(teams_fut, presence_fut);
    let teams_err = match teams_stage_res {
        Ok(err_opt) => err_opt,
        Err(db_err) => return Err(db_err),
    };
    if let Some(err) = teams_err {
        if let Some(prev) = first_err.take() {
            first_err = Some(anyhow::anyhow!("{prev}; Teams refresh failed: {err}"));
        } else {
            first_err = Some(err);
        }
    }

    if let Some(err) = first_err {
        return Err(err);
    }
    Ok(())
}

async fn clear_channel_delta_cursor(
    storage: &Storage,
    conv_key: &str,
    expected: Option<SyncCursor>,
) -> Result<Option<SyncCursor>> {
    let cleared = SyncCursor {
        scope_key: conv_key.to_string(),
        delta_link: None,
        oldest_msg_ts: expected.as_ref().and_then(|cursor| cursor.oldest_msg_ts.clone()),
        synced_at: chrono::Utc::now().to_rfc3339(),
    };
    run_storage(storage, move |st| {
        let outcome = st.apply_channel_delta(expected.as_ref(), &cleared, &[], &HashSet::new())?;
        Ok(outcome.map(|_| cleared))
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn refresh_channel_delta_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    team_id: &str,
    channel_id: &str,
    conv_key: &str,
    conv_title: &str,
    mark_unread_on_new: bool,
    live_focus_rx: Option<&watch::Receiver<FocusTarget>>,
) -> Result<ChannelDeltaOutcome> {
    let key = conv_key.to_string();
    let mut cursor = run_storage(storage, move |st| st.get_sync_cursor(&key)).await?;
    let delta_link = cursor
        .as_ref()
        .and_then(|c| c.delta_link.as_deref())
        .map(str::trim)
        .filter(|link| !link.is_empty());
    let requested_at = chrono::Utc::now();
    let (messages, next_link, incremental) = match graph
        .sync_channel_messages_delta(team_id, channel_id, delta_link)
        .await
    {
        Ok((messages, next_link)) => {
            let incremental = delta_link.is_some();
            let next_link = next_link.or_else(|| delta_link.map(str::to_string));
            (messages, next_link, incremental)
        }
        Err(err) if is_expired_delta_error(&err) => {
            let Some(cleared) = clear_channel_delta_cursor(storage, conv_key, cursor).await? else {
                return Ok(ChannelDeltaOutcome::default());
            };
            cursor = Some(cleared);
            let target = ConversationTarget::Channel {
                team_id: team_id.to_string(),
                channel_id: channel_id.to_string(),
            };
            let (messages, _) = graph.list_messages_page(&target, 50, None).await?;
            (messages, None, false)
        }
        Err(err) => return Err(err),
    };
    let oldest_ts = messages
        .iter()
        .map(|m| m.created_at.as_str())
        .filter(|ts| !ts.is_empty())
        .min()
        .map(str::to_string)
        .or_else(|| cursor.as_ref().and_then(|c| c.oldest_msg_ts.clone()));
    let new_cursor = SyncCursor {
        scope_key: conv_key.to_string(),
        delta_link: next_link,
        oldest_msg_ts: oldest_ts,
        synced_at: requested_at.to_rfc3339(),
    };

    // An original post can arrive late. Use the last request time only to distinguish
    // a fresh post that has already changed from an edit to uncached channel history.
    let last_request_at = cursor
        .as_ref()
        .and_then(|c| chrono::DateTime::parse_from_rfc3339(&c.synced_at).ok());
    let live_focus_rx = live_focus_rx.cloned();
    let applied = run_storage(storage, move |st| {
        let focused_now = live_focus_rx.as_ref().is_some_and(|rx| {
            rx.borrow()
                .target
                .as_ref()
                .is_some_and(|target| target.conv_key() == new_cursor.scope_key)
        });
        let eligible_unread: HashSet<&str> = messages
            .iter()
            .filter(|message| {
                if !incremental || !mark_unread_on_new || focused_now || message.is_me || message.deleted {
                    return false;
                }
                let Ok(created) = chrono::DateTime::parse_from_rfc3339(&message.created_at) else {
                    return false;
                };
                let original = message.reactions.is_empty()
                    && message.last_modified_at.as_deref().is_none_or(|modified| {
                        chrono::DateTime::parse_from_rfc3339(modified)
                            .is_ok_and(|modified| modified == created)
                    });
                original || last_request_at.is_some_and(|cutoff| created >= cutoff)
            })
            .map(|message| message.msg_id.as_str())
            .collect();
        let Some(outcome) =
            st.apply_channel_delta(cursor.as_ref(), &new_cursor, &messages, &eligible_unread)?
        else {
            return Ok(None);
        };
        let newly_inserted = outcome
            .inserted_ids
            .iter()
            .filter(|id| eligible_unread.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        let new_messages = filter_changed_messages_ref(&messages, &newly_inserted);
        let changed = filter_changed_messages(messages, &outcome.changed_ids);
        Ok(Some((outcome, changed, new_messages)))
    })
    .await?;
    let Some((outcome, changed_messages, newly_inserted)) = applied else {
        return Ok(ChannelDeltaOutcome::default());
    };

    let any_summary_changed = !newly_inserted.is_empty() || !outcome.preview_updated_convs.is_empty();
    if !outcome.changed_ids.is_empty() {
        let _ = event_tx.send(CoreEvent::MessagesUpdated {
            conv_key: conv_key.to_string(),
            parent_msg_id: None,
            messages: changed_messages,
            notify_candidate: build_notification_candidate(conv_key, conv_title, &newly_inserted),
            changed_msg_ids: Some(outcome.changed_ids),
        });
    }
    Ok(ChannelDeltaOutcome { any_summary_changed })
}

async fn sync_channel_history(
    team_id: &str,
    channel_id: &str,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
) -> Result<()> {
    let target = ConversationTarget::Channel {
        team_id: team_id.to_string(),
        channel_id: channel_id.to_string(),
    };
    let conv_key = target.conv_key();
    let (messages, next_link) = graph.list_messages_page(&target, 50, None).await?;
    let msgs_for_db = messages.clone();
    let key_for_db = conv_key.clone();
    run_storage(storage, move |st| {
        st.upsert_messages(&msgs_for_db)?;
        save_initial_history_cursor_if_absent(st, &key_for_db, &msgs_for_db, next_link)
    })
    .await?;
    let _ = event_tx.send(CoreEvent::MessagesUpdated {
        conv_key,
        parent_msg_id: None,
        messages,
        notify_candidate: None,
        changed_msg_ids: None,
    });
    Ok(())
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn collect_dirty_chat_ids(
    fresh_chats: &[ConversationSummary],
    cached_by_key: &HashMap<String, ConversationSummary>,
) -> Vec<String> {
    let mut dirty = Vec::new();
    for chat in fresh_chats {
        let ConversationTarget::Chat { chat_id } = &chat.target else {
            continue;
        };
        let is_dirty = match cached_by_key.get(&chat.conv_key) {
            None => chat.last_msg_id.is_some() || chat.last_updated_at.is_some(),
            Some(cached) => {
                (chat.last_msg_id.is_some() && chat.last_msg_id != cached.last_msg_id)
                    || (chat.last_updated_at.is_some()
                        && chat.last_updated_at != cached.last_updated_at)
                    || (chat.last_msg_preview != cached.last_msg_preview)
            }
        };
        if is_dirty {
            dirty.push(chat_id.clone());
        }
    }
    dirty
}

pub(crate) fn build_notification_candidate(
    conv_key: &str,
    conv_title: &str,
    messages: &[MessageItem],
) -> Option<NotificationCandidate> {
    let newest = messages
        .iter()
        .filter(|m| !m.deleted)
        .max_by(|a, b| a.created_at.cmp(&b.created_at).then(a.msg_id.cmp(&b.msg_id)))?;
    if newest.is_me {
        return None;
    }
    let preview = newest
        .body_plain
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Some(NotificationCandidate {
        conv_key: conv_key.to_string(),
        conv_title: conv_title.to_string(),
        sender: newest.author_name.clone(),
        preview,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthManager, StoredTokens};

    fn chat_summary(id: &str, last_msg_id: Option<&str>, updated: Option<&str>) -> ConversationSummary {
        let target = ConversationTarget::Chat {
            chat_id: id.to_string(),
        };
        ConversationSummary {
            conv_key: target.conv_key(),
            target,
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: format!("Chat {id}"),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: last_msg_id.map(str::to_string),
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: updated.map(str::to_string),
            pinned: false,
            peer_user_id: None,
            web_url: None,
        }
    }

    fn sample_msg(conv_key: &str, msg_id: &str, created_at: &str) -> MessageItem {
        MessageItem {
            msg_key: MessageItem::make_msg_key(conv_key, msg_id),
            conv_key: conv_key.to_string(),
            msg_id: msg_id.to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("u2".to_string()),
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: created_at.to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: "<p>message</p>".to_string(),
            body_plain: "message".to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        }
    }

    #[test]
    fn test_collect_dirty_chat_ids_and_notification_candidate() {
        let fresh = vec![
            chat_summary("c1", Some("m2"), Some("2026-11-15T10:05:00Z")),
            chat_summary("c2", Some("m1"), Some("2026-11-15T09:00:00Z")),
            chat_summary("c3", Some("m9"), Some("2026-11-15T11:00:00Z")),
        ];
        let mut cached = HashMap::new();
        cached.insert(
            "chat:c1".to_string(),
            chat_summary("c1", Some("m1"), Some("2026-11-15T10:00:00Z")),
        );
        cached.insert(
            "chat:c2".to_string(),
            chat_summary("c2", Some("m1"), Some("2026-11-15T09:00:00Z")),
        );

        let dirty = collect_dirty_chat_ids(&fresh, &cached);
        assert_eq!(dirty, vec!["c1".to_string(), "c3".to_string()]);

        let mut msg_other = sample_msg("chat:c1", "m2", "2026-11-15T10:05:00Z");
        msg_other.body_plain = "New\nmessage".to_string();
        let cand = build_notification_candidate("chat:c1", "Alice", &[msg_other.clone()]).unwrap();
        assert_eq!(cand.sender, "Alice");
        assert_eq!(cand.preview, "New message");

        let mut msg_me = msg_other;
        msg_me.is_me = true;
        assert!(build_notification_candidate("chat:c1", "Alice", &[msg_me]).is_none());
    }

    #[tokio::test]
    async fn test_spawn_sync_engine_controller_and_shutdown() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(config, graph, storage, event_tx);
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c1".to_string(),
            }),
            None,
        );
        ctrl.request(SyncCommand::Shutdown);
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn test_spawn_sync_engine_two_lane_routing_and_drop_shutdown() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(config, graph, storage, event_tx);

        // Coalesce rapid focus switches and verify both lanes process commands without deadlocking
        ctrl.request(SyncCommand::RefreshConversations);
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c-first".to_string(),
            }),
            None,
        );
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c-latest".to_string(),
            }),
            None,
        );
        ctrl.request(SyncCommand::SyncActiveNow);

        // Wait briefly for Background Poll Lane to emit SyncStateChanged and Fast Lane to process
        let mut saw_conv_refresh_state = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), event_rx.recv()).await {
                Ok(Some(CoreEvent::SyncStateChanged { summary, .. }))
                    if summary.contains("conversations") =>
                {
                    saw_conv_refresh_state = true;
                    break;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_conv_refresh_state);

        // Dropping controller closes nudge_tx + focus_tx and shuts down both lanes cleanly
        drop(ctrl);
        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "both sync lanes should terminate when controller is dropped");
    }

    #[test]
    fn test_idle_backoff_multiplier_and_immediate_activity_reset() {
        assert_eq!(idle_multiplier(Duration::from_secs(0)), 1);
        assert_eq!(idle_multiplier(Duration::from_secs(119)), 1);
        assert_eq!(idle_multiplier(Duration::from_secs(120)), 4);
        assert_eq!(idle_multiplier(Duration::from_secs(599)), 4);
        assert_eq!(idle_multiplier(Duration::from_secs(600)), 10);
        assert_eq!(idle_multiplier(Duration::from_secs(1_800)), 10);

        let epoch = Instant::now();
        let state = SyncSharedState::with_epoch(epoch);
        let mut tracker = PeriodicBackoffTracker::default();

        // Active (< 120s): every tick runs (1x)
        let (idle, seq) = state.idle_snapshot(epoch + Duration::from_secs(30));
        assert!(tracker.on_tick(idle, seq));
        assert!(tracker.on_tick(idle, seq));

        // Idle (120s .. 600s): 4x — skips 3 out of 4 ticks
        let (idle_4x, seq_4x) = state.idle_snapshot(epoch + Duration::from_secs(150));
        let results_4x: Vec<bool> = (0..8)
            .map(|_| tracker.on_tick(idle_4x, seq_4x))
            .collect();
        assert_eq!(
            results_4x,
            vec![false, false, false, true, false, false, false, true]
        );

        // Deep idle (>= 600s): 10x — skips 9 out of 10 ticks
        let mut deep_tracker = PeriodicBackoffTracker::default();
        let (idle_10x, seq_10x) = state.idle_snapshot(epoch + Duration::from_secs(600));
        let results_10x: Vec<bool> = (0..20)
            .map(|_| deep_tracker.on_tick(idle_10x, seq_10x))
            .collect();
        let expected_10x: Vec<bool> = (1..=20).map(|i| i % 10 == 0).collect();
        assert_eq!(results_10x, expected_10x);

        // User activity arrives at 610s -> immediate reset to 1x active!
        state.record_activity(epoch + Duration::from_secs(610));
        let (idle_after, seq_after) = state.idle_snapshot(epoch + Duration::from_secs(611));
        assert_eq!(idle_multiplier(idle_after), 1);
        assert!(deep_tracker.on_tick(idle_after, seq_after));
        assert!(tracker.on_tick(idle_after, seq_after));
    }

    #[tokio::test]
    async fn test_sync_controller_activity_and_shutdown_cleanly() {
        let config = AppConfig::default();
        let auth = AuthManager::new(config.clone()).unwrap();
        let graph = GraphClient::new(config.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(config, graph, storage, event_tx);
        ctrl.notify_activity();
        ctrl.request(SyncCommand::Shutdown);

        let closed = tokio::time::timeout(Duration::from_secs(2), async {
            while event_rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok());
    }

    #[test]
    fn test_maybe_shrink_on_idle_transition_state_machine() {
        let storage = Storage::open_in_memory().unwrap();
        let mut was_idle = false;

        // Active duration stays false
        maybe_shrink_on_idle(
            Duration::from_secs(IDLE_THRESHOLD_SECS - 1),
            &mut was_idle,
            &storage,
        );
        assert!(!was_idle);

        // Crossing threshold sets was_idle = true and runs shrink_memory
        maybe_shrink_on_idle(
            Duration::from_secs(IDLE_THRESHOLD_SECS),
            &mut was_idle,
            &storage,
        );
        assert!(was_idle);

        // Staying idle keeps was_idle = true
        maybe_shrink_on_idle(
            Duration::from_secs(DEEP_IDLE_THRESHOLD_SECS),
            &mut was_idle,
            &storage,
        );
        assert!(was_idle);

        // Returning to active resets was_idle = false
        maybe_shrink_on_idle(Duration::from_secs(5), &mut was_idle, &storage);
        assert!(!was_idle);
    }

    #[test]
    fn test_initial_history_and_chats_list_cursor_preservation() {
        let storage = Storage::open_in_memory().unwrap();
        let conv_key = "chat:c1";
        let msgs = vec![
            sample_msg(conv_key, "m1", "2026-11-15T09:00:00Z"),
            sample_msg(conv_key, "m2", "2026-11-15T10:00:00Z"),
        ];

        // 1) Initial save with nextLink records delta_link + oldest_msg_ts
        let next_url = "https://graph.microsoft.com/v1.0/chats/c1/messages?$skiptoken=page2";
        save_initial_history_cursor_if_absent(
            &storage,
            conv_key,
            &msgs,
            Some(next_url.to_string()),
        )
        .unwrap();
        let saved = storage
            .get_sync_cursor(&SyncCursor::history_scope(conv_key))
            .unwrap()
            .unwrap();
        assert_eq!(saved.delta_link.as_deref(), Some(next_url));
        assert_eq!(
            saved.oldest_msg_ts.as_deref(),
            Some("2026-11-15T09:00:00Z")
        );
        assert!(!saved.is_history_eof());

        // 2) Subsequent sync must NOT overwrite an existing deeper history cursor
        let deeper_cursor = SyncCursor {
            scope_key: SyncCursor::history_scope(conv_key),
            delta_link: Some("https://graph.microsoft.com/v1.0/chats/c1/messages?$skiptoken=page3".to_string()),
            oldest_msg_ts: Some("2026-11-10T00:00:00Z".to_string()),
            synced_at: "2026-11-15T10:05:00Z".to_string(),
        };
        storage.save_sync_cursor(&deeper_cursor).unwrap();
        save_initial_history_cursor_if_absent(&storage, conv_key, &msgs, None).unwrap();
        let preserved = storage
            .get_sync_cursor(&SyncCursor::history_scope(conv_key))
            .unwrap()
            .unwrap();
        assert_eq!(preserved.delta_link, deeper_cursor.delta_link);
        assert_eq!(preserved.oldest_msg_ts, deeper_cursor.oldest_msg_ts);

        // 3) Conversation with no nextLink records HISTORY_EOF immediately
        let eof_key = "chat:c-eof";
        save_initial_history_cursor_if_absent(&storage, eof_key, &msgs, None).unwrap();
        let eof_cursor = storage
            .get_sync_cursor(&SyncCursor::history_scope(eof_key))
            .unwrap()
            .unwrap();
        assert!(eof_cursor.is_history_eof());
        assert_eq!(
            eof_cursor.oldest_msg_ts.as_deref(),
            Some(SyncCursor::HISTORY_EOF)
        );

        // 4) CHATS_LIST_SCOPE saves initial next_link and does not overwrite existing cursor
        let chats_next = "https://graph.microsoft.com/v1.0/me/chats?$skiptoken=chats2";
        save_initial_chats_list_cursor(&storage, Some(chats_next.to_string())).unwrap();
        let chats_cursor = storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .unwrap()
            .unwrap();
        assert_eq!(chats_cursor.delta_link.as_deref(), Some(chats_next));
        assert!(!chats_cursor.is_history_eof());

        save_initial_chats_list_cursor(&storage, None).unwrap();
        let chats_preserved = storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .unwrap()
            .unwrap();
        assert_eq!(chats_preserved.delta_link.as_deref(), Some(chats_next));
    }

    #[tokio::test]
    async fn test_sync_active_focus_and_channel_history_record_history_cursor() {
        use axum::{routing::get, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");

        let app = Router::new()
            .route(
                "/v1.0/chats/c1/messages",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "m1",
                            "createdDateTime": "2026-11-16T09:00:00Z",
                            "messageType": "message",
                            "importance": "normal",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "text", "content": "Hello" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch1/messages",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "cm1",
                            "createdDateTime": "2026-11-16T09:05:00Z",
                            "messageType": "message",
                            "importance": "normal",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "text", "content": "Channel post" }
                        }]
                    }))
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: base_url,
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "test-tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();

        let focus = FocusTarget {
            target: Some(ConversationTarget::Chat {
                chat_id: "c1".to_string(),
            }),
            thread_root_id: None,
        };
        sync_active_focus(&focus, &graph, &storage, &event_tx)
            .await
            .unwrap();
        assert!(storage
            .get_sync_cursor(&SyncCursor::history_scope("chat:c1"))
            .unwrap()
            .is_some());

        sync_channel_history("t1", "ch1", &graph, &storage, &event_tx)
            .await
            .unwrap();
        assert!(storage
            .get_sync_cursor(&SyncCursor::history_scope("chan:t1:ch1"))
            .unwrap()
            .is_some());

        server.abort();
    }

    #[test]
    fn test_classify_sync_error_and_expired_delta() {
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!(
                "Graph API error (GET https://graph.microsoft.com/v1.0/me/presence): 403 Forbidden [Authorization_RequestDenied]"
            )),
            SyncErrorKind::Forbidden403
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!("AclCheckFailed on channel")),
            SyncErrorKind::Forbidden403
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!("Insufficient privileges")),
            SyncErrorKind::Forbidden403
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!(
                "Graph API error (GET https://graph.microsoft.com/v1.0/me/chats): 401 Unauthorized"
            )),
            SyncErrorKind::Unauthorized401
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!("Token expired (exp: 123, auth_mode=env_token)")),
            SyncErrorKind::Unauthorized401
        );
        // URL containing "403" must not misclassify a 500 response as 403 Forbidden.
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!(
                "Graph API error (GET https://graph.microsoft.com/v1.0/chats/19:403abc/messages): 500 Internal Server Error"
            )),
            SyncErrorKind::Transient
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!("request timed out")),
            SyncErrorKind::Transient
        );
        // A "403"/"401" in the error body or a message id does not reclassify another status.
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!(
                "Graph API error (GET https://graph.microsoft.com/v1.0/chats/c1/messages/1700000403123): 500 Internal Server Error [generalException] request 403401 failed"
            )),
            SyncErrorKind::Transient
        );
        // Context added in front of the Graph error keeps the status parseable.
        assert_eq!(
            classify_sync_error(
                &anyhow::anyhow!(
                    "Graph API error (GET https://graph.microsoft.com/v1.0/me/chats): 403 Forbidden"
                )
                .context("refresh failed")
            ),
            SyncErrorKind::Forbidden403
        );

        // 404 is its own class, decided by the response status only.
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!(
                "Graph API error (GET https://graph.microsoft.com/v1.0/teams/t1/channels/c1/messages/delta): 404 Not Found [NotFound] channel was deleted"
            )),
            SyncErrorKind::NotFound404
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!(
                "Graph API error (GET https://graph.microsoft.com/v1.0/chats/19:404abc/messages/1700000404123): 500 Internal Server Error [generalException] request 404 failed"
            )),
            SyncErrorKind::Transient
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!("Failed to parse message 404 of the page")),
            SyncErrorKind::Transient
        );

        assert!(is_expired_delta_error(&anyhow::anyhow!(
            "Graph API error (GET https://graph.microsoft.com/v1.0/teams/t1/channels/c1/messages/delta): 410 Gone [resyncRequired]"
        )));
        assert!(is_expired_delta_error(&anyhow::anyhow!(
            "400 Bad Request [syncStateNotFound]"
        )));
        assert!(!is_expired_delta_error(&anyhow::anyhow!("403 Forbidden")));
        assert!(!is_expired_delta_error(&anyhow::anyhow!("401 Unauthorized")));
        assert!(!is_expired_delta_error(&anyhow::anyhow!("429 Too Many Requests")));
    }

    #[test]
    fn test_error_backoff_state_exponential_jitter_and_recovery() {
        assert_eq!(ErrorBackoffState::compute_backoff_ms(1_000, 1, 1.0), 2_000);
        assert_eq!(ErrorBackoffState::compute_backoff_ms(1_000, 6, 1.0), 64_000);
        assert_eq!(
            ErrorBackoffState::compute_backoff_ms(10_000, 10, 1.0),
            300_000
        );
        let jittered = ErrorBackoffState::compute_backoff_ms(1_000, 1, 0.8);
        assert_eq!(jittered, 1_600);

        let mut backoff = ErrorBackoffState::new(1_000);
        let t0 = Instant::now();
        assert!(!backoff.is_degraded());
        assert!(backoff.record_transient_failure(t0));
        assert!(backoff.is_degraded());
        assert!(backoff.is_backed_off(t0));
        // Subsequent failure stays degraded without re-emitting transition.
        assert!(!backoff.record_transient_failure(t0 + Duration::from_secs(5)));
        assert!(backoff.record_success());
        assert!(!backoff.is_degraded());
        assert!(!backoff.is_backed_off(t0));
    }

    #[tokio::test]
    async fn test_channel_delta_fallback_respects_error_kind_and_preserves_cursor() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let delta_mode = Arc::new(AtomicUsize::new(0)); // 0=403, 1=429, 2=410, 3=ok_no_link
        let fallback_hits = Arc::new(AtomicUsize::new(0));

        let dm = Arc::clone(&delta_mode);
        let fh = Arc::clone(&fallback_hits);

        let app = Router::new()
            .route(
                "/v1.0/teams/t1/channels/ch1/messages/delta",
                get(move || {
                    let dm = Arc::clone(&dm);
                    async move {
                        match dm.load(Ordering::SeqCst) {
                            0 => (
                                StatusCode::FORBIDDEN,
                                Json(serde_json::json!({ "error": { "code": "Forbidden", "message": "Denied" } })),
                            ),
                            1 => (
                                StatusCode::TOO_MANY_REQUESTS,
                                Json(serde_json::json!({ "error": { "code": "TooManyRequests", "message": "Throttled" } })),
                            ),
                            2 => (
                                StatusCode::GONE,
                                Json(serde_json::json!({ "error": { "code": "resyncRequired", "message": "Expired" } })),
                            ),
                            _ => (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [{
                                        "id": "cm-delta",
                                        "createdDateTime": "2026-11-16T09:10:00Z",
                                        "messageType": "message",
                                        "importance": "normal",
                                        "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                        "body": { "contentType": "text", "content": "Delta msg" }
                                    }]
                                })),
                            ),
                        }
                    }
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch1/messages",
                get(move || {
                    let fh = Arc::clone(&fh);
                    async move {
                        fh.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [{
                                    "id": "cm-fallback",
                                    "createdDateTime": "2026-11-16T09:05:00Z",
                                    "messageType": "message",
                                    "importance": "normal",
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "text", "content": "Fallback post" }
                                }]
                            })),
                        )
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();

        let focus = FocusTarget {
            target: Some(ConversationTarget::Channel {
                team_id: "t1".to_string(),
                channel_id: "ch1".to_string(),
            }),
            thread_root_id: None,
        };

        // 403 must NOT trigger list_messages_page fallback.
        delta_mode.store(0, Ordering::SeqCst);
        let err_403 = sync_active_focus(&focus, &graph, &storage, &event_tx)
            .await
            .unwrap_err();
        assert_eq!(classify_sync_error(&err_403), SyncErrorKind::Forbidden403);
        assert_eq!(fallback_hits.load(Ordering::SeqCst), 0);

        // 410 resyncRequired MUST trigger list_messages_page fallback and clear stale delta_link.
        storage
            .save_sync_cursor(&SyncCursor {
                scope_key: "chan:t1:ch1".to_string(),
                delta_link: Some(format!("http://{addr}/v1.0/teams/t1/channels/ch1/messages/delta?state=stale")),
                oldest_msg_ts: None,
                synced_at: "2026-11-16T09:00:00Z".to_string(),
            })
            .unwrap();
        delta_mode.store(2, Ordering::SeqCst);
        sync_active_focus(&focus, &graph, &storage, &event_tx)
            .await
            .unwrap();
        assert_eq!(fallback_hits.load(Ordering::SeqCst), 1);
        assert!(storage
            .get_sync_cursor("chan:t1:ch1")
            .unwrap()
            .unwrap()
            .delta_link
            .is_none());

        // Ok with new_delta_link = None must preserve an existing delta_link.
        let keep_url = format!("http://{addr}/v1.0/teams/t1/channels/ch1/messages/delta?state=keep");
        storage
            .save_sync_cursor(&SyncCursor {
                scope_key: "chan:t1:ch1".to_string(),
                delta_link: Some(keep_url.clone()),
                oldest_msg_ts: None,
                synced_at: "2026-11-16T09:00:00Z".to_string(),
            })
            .unwrap();
        delta_mode.store(3, Ordering::SeqCst);
        sync_active_focus(&focus, &graph, &storage, &event_tx)
            .await
            .unwrap();
        assert_eq!(
            storage
                .get_sync_cursor("chan:t1:ch1")
                .unwrap()
                .unwrap()
                .delta_link
                .as_deref(),
            Some(keep_url.as_str())
        );

        server.abort();
    }

    #[tokio::test]
    async fn test_quiet_background_poll_403_circuit_breaker_and_transient_retry_spinner() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let c403_hits = Arc::new(AtomicUsize::new(0));
        let c500_hits = Arc::new(AtomicUsize::new(0));

        let h403 = Arc::clone(&c403_hits);
        let h500 = Arc::clone(&c500_hits);

        let app = Router::new()
            .route(
                "/v1.0/chats/c403/messages",
                get(move || {
                    let h403 = Arc::clone(&h403);
                    async move {
                        h403.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::FORBIDDEN,
                            Json(serde_json::json!({ "error": { "code": "Forbidden", "message": "No access" } })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/chats/c500/messages",
                get(move || {
                    let h500 = Arc::clone(&h500);
                    async move {
                        let n = h500.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({ "error": { "code": "InternalServerError", "message": "Boom" } })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [{
                                        "id": "m1",
                                        "createdDateTime": "2026-11-16T09:00:00Z",
                                        "messageType": "message",
                                        "importance": "normal",
                                        "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                        "body": { "contentType": "text", "content": "Recovered" }
                                    }]
                                })),
                            )
                        }
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            active_poll_ms: 250,
            chat_poll_ms: 60_000,
            channel_poll_ms: 60_000,
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        let ctrl = spawn_sync_engine(cfg, graph, storage, event_tx);

        // 1) Focus on 403 room: hit once, circuit-breaker skips subsequent 250ms ticks, zero SyncError/POLL_RETRY_SUMMARY.
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c403".to_string(),
            }),
            None,
        );
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(c403_hits.load(Ordering::SeqCst), 1);

        // Explicit SyncActiveNow clears restricted_convs and tries once more (still 403 -> stops again).
        ctrl.request(SyncCommand::SyncActiveNow);
        tokio::time::sleep(Duration::from_millis(650)).await;
        assert_eq!(c403_hits.load(Ordering::SeqCst), 2);

        // 2) Focus on 500 -> 200 room: emits POLL_RETRY_SUMMARY quietly, then recovers to "Synced".
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c500".to_string(),
            }),
            None,
        );
        let mut saw_retry_summary = false;
        let mut saw_recovered_synced = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(150), event_rx.recv()).await {
                Ok(Some(CoreEvent::SyncError(err))) => {
                    panic!("background poll must not emit SyncError: {err}");
                }
                Ok(Some(CoreEvent::SyncStateChanged { busy: false, summary })) => {
                    if summary == POLL_RETRY_SUMMARY {
                        saw_retry_summary = true;
                    } else if saw_retry_summary && summary == "Synced" {
                        saw_recovered_synced = true;
                        break;
                    }
                }
                Ok(Some(_)) => continue,
                Ok(None) => break,
                Err(_) => {
                    if saw_retry_summary && !saw_recovered_synced {
                        continue;
                    }
                }
            }
        }
        assert!(saw_retry_summary, "expected POLL_RETRY_SUMMARY on transient failure");
        assert!(saw_recovered_synced, "expected Synced after transient recovery");

        ctrl.request(SyncCommand::Shutdown);
        server.abort();
    }

    #[tokio::test]
    async fn test_chat_sync_producers_emit_exact_changed_msg_ids_and_notifications() {
        use axum::{routing::{get, post}, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let chat_stage = Arc::new(AtomicUsize::new(0));
        let stage_messages = Arc::clone(&chat_stage);
        let stage_chats = Arc::clone(&chat_stage);
        let stage_batch = Arc::clone(&chat_stage);

        let app = Router::new()
            .route(
                "/v1.0/chats/c1/messages",
                get(move || {
                    let stage = Arc::clone(&stage_messages);
                    async move {
                        let s = stage.load(Ordering::SeqCst);
                        let value = match s {
                            0 => vec![serde_json::json!({
                                "id": "m1",
                                "createdDateTime": "2026-11-20T01:00:00Z",
                                "messageType": "message",
                                "importance": "normal",
                                "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                "body": { "contentType": "html", "content": "<p>one</p>" }
                            })],
                            1 => vec![
                                serde_json::json!({
                                    "id": "m2",
                                    "createdDateTime": "2026-11-20T02:00:00Z",
                                    "messageType": "message",
                                    "importance": "normal",
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "html", "content": "<p>two</p>" }
                                }),
                                serde_json::json!({
                                    "id": "m1",
                                    "createdDateTime": "2026-11-20T01:00:00Z",
                                    "messageType": "message",
                                    "importance": "normal",
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "html", "content": "<p>one</p>" }
                                }),
                            ],
                            _ => vec![
                                serde_json::json!({
                                    "id": "m2",
                                    "createdDateTime": "2026-11-20T02:00:00Z",
                                    "lastModifiedDateTime": "2026-11-20T02:05:00Z",
                                    "deletedDateTime": "2026-11-20T02:05:00Z",
                                    "messageType": "message",
                                    "importance": "normal",
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "html", "content": "" }
                                }),
                                serde_json::json!({
                                    "id": "m1",
                                    "createdDateTime": "2026-11-20T01:00:00Z",
                                    "messageType": "message",
                                    "importance": "normal",
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "html", "content": "<p>one</p>" }
                                }),
                            ],
                        };
                        Json(serde_json::json!({ "value": value }))
                    }
                }),
            )
            .route(
                "/v1.0/me/chats",
                get(move || {
                    let stage = Arc::clone(&stage_chats);
                    async move {
                        let s = stage.load(Ordering::SeqCst);
                        let last_id = if s >= 3 { "m3" } else { "m2" };
                        let updated = if s >= 3 {
                            "2026-11-20T03:00:00Z"
                        } else {
                            "2026-11-20T02:00:00Z"
                        };
                        Json(serde_json::json!({
                            "value": [{
                                "id": "c1",
                                "chatType": "oneOnOne",
                                "topic": null,
                                "lastUpdatedDateTime": updated,
                                "members": [
                                    { "userId": "u1", "displayName": "Me" },
                                    { "userId": "u2", "displayName": "Alice" }
                                ],
                                "lastMessagePreview": {
                                    "id": last_id,
                                    "createdDateTime": updated,
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "text", "content": "three" }
                                }
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/$batch",
                post(move |Json(req): Json<serde_json::Value>| {
                    let _stage = Arc::clone(&stage_batch);
                    async move {
                        let req_id = req["requests"][0]["id"].as_str().unwrap_or("0");
                        Json(serde_json::json!({
                            "responses": [{
                                "id": req_id,
                                "status": 200,
                                "body": {
                                    "value": [
                                        {
                                            "id": "m3",
                                            "createdDateTime": "2026-11-20T03:00:00Z",
                                            "messageType": "message",
                                            "importance": "normal",
                                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                            "body": { "contentType": "html", "content": "<p>three from batch</p>" }
                                        },
                                        {
                                            "id": "m1",
                                            "createdDateTime": "2026-11-20T01:00:00Z",
                                            "messageType": "message",
                                            "importance": "normal",
                                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                            "body": { "contentType": "html", "content": "<p>one</p>" }
                                        }
                                    ]
                                }
                            }]
                        }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth)
            .unwrap()
            .with_my_user_id("u1");
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_conversations(&[chat_summary("c1", Some("m1"), Some("2026-11-20T01:00:00Z"))])
            .unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let focus = FocusTarget {
            target: Some(ConversationTarget::Chat {
                chat_id: "c1".to_string(),
            }),
            thread_root_id: None,
        };
        let mut sigs = HashMap::new();

        // 1) Initial immediate focus poll inserts m1 -> changed_msg_ids == Some(["m1"]).
        sync_active_focus_with_state(&focus, &graph, &storage, &event_tx, &mut sigs, false)
            .await
            .unwrap();
        match event_rx.try_recv().unwrap() {
            CoreEvent::MessagesUpdated {
                conv_key,
                messages,
                changed_msg_ids,
                ..
            } => {
                assert_eq!(conv_key, "chat:c1");
                assert_eq!(changed_msg_ids, Some(vec!["m1".to_string()]));
                assert_eq!(messages.len(), 1);
            }
            other => panic!("expected MessagesUpdated, got {other:?}"),
        }

        // 2) Immediate focus re-poll with 0 DB changes -> changed_msg_ids == Some([]), messages == [].
        sync_active_focus_with_state(&focus, &graph, &storage, &event_tx, &mut sigs, false)
            .await
            .unwrap();
        match event_rx.try_recv().unwrap() {
            CoreEvent::MessagesUpdated {
                messages,
                changed_msg_ids,
                ..
            } => {
                assert_eq!(changed_msg_ids, Some(Vec::new()));
                assert!(messages.is_empty());
            }
            other => panic!("expected MessagesUpdated, got {other:?}"),
        }

        // 3) Periodic poll with 1 new message (m2) -> changed_msg_ids == Some(["m2"]) only.
        chat_stage.store(1, Ordering::SeqCst);
        sync_active_focus_with_state(&focus, &graph, &storage, &event_tx, &mut sigs, true)
            .await
            .unwrap();
        match event_rx.try_recv().unwrap() {
            CoreEvent::MessagesUpdated {
                messages,
                changed_msg_ids,
                ..
            } => {
                assert_eq!(changed_msg_ids, Some(vec!["m2".to_string()]));
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].msg_id, "m2");
            }
            other => panic!("expected MessagesUpdated, got {other:?}"),
        }

        // 4) Periodic poll with soft-delete on m2 -> changed_msg_ids == Some(["m2"]).
        chat_stage.store(2, Ordering::SeqCst);
        sync_active_focus_with_state(&focus, &graph, &storage, &event_tx, &mut sigs, true)
            .await
            .unwrap();
        match event_rx.try_recv().unwrap() {
            CoreEvent::MessagesUpdated {
                messages,
                changed_msg_ids,
                ..
            } => {
                assert_eq!(changed_msg_ids, Some(vec!["m2".to_string()]));
                assert_eq!(messages.len(), 1);
                assert!(messages[0].deleted);
            }
            other => panic!("expected MessagesUpdated, got {other:?}"),
        }

        // 5) Background tier-2 batch poll for dirty chat emits changed_msg_ids == Some(["m3"])
        //    and preserves notify_candidate.
        chat_stage.store(3, Ordering::SeqCst);
        let mut chat_cache = HashMap::new();
        let mut seeded = false;
        let mut cursor_init = false;
        poll_chats_tier2_with_cache(
            &graph,
            &storage,
            &event_tx,
            &mut chat_cache,
            &mut seeded,
            &mut cursor_init,
        )
        .await
        .unwrap();
        let mut saw_batch_update = false;
        while let Ok(ev) = event_rx.try_recv() {
            if let CoreEvent::MessagesUpdated {
                conv_key,
                messages,
                notify_candidate,
                changed_msg_ids,
                ..
            } = ev
            {
                assert_eq!(conv_key, "chat:c1");
                assert_eq!(changed_msg_ids, Some(vec!["m3".to_string()]));
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].msg_id, "m3");
                let cand = notify_candidate.expect("expected notification candidate");
                assert_eq!(cand.sender, "Alice");
                assert_eq!(cand.preview, "three from batch");
                saw_batch_update = true;
            }
        }
        assert!(saw_batch_update, "expected tier-2 MessagesUpdated event");

        server.abort();
    }

    #[tokio::test]
    async fn test_rolling_batch_inactive_edit_reaction_signature_and_channel_refresh() {
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        use std::sync::atomic::AtomicUsize;

        let stage = Arc::new(AtomicUsize::new(0));
        let st_batch = Arc::clone(&stage);
        let st_active = Arc::clone(&stage);
        let st_delta = Arc::clone(&stage);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let delta_base = format!("http://{addr}/v1.0/teams/t1/channels/ch1/messages/delta");

        let app = Router::new()
            .route(
                "/v1.0/me/chats",
                get(|| async {
                    // Keep lastMessagePreview (id, body, AND lastUpdatedDateTime) identical
                    // across stages to prove rolling batch still catches edits/deletes on
                    // inactive chats.
                    let preview_body = "<p>bg-last-original</p>";
                    Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "c-bg-1",
                                    "chatType": "oneOnOne",
                                    "lastUpdatedDateTime": "2026-11-20T03:02:00Z",
                                    "members": [
                                        { "userId": "u1", "displayName": "Me" },
                                        { "userId": "u2", "displayName": "Alice" }
                                    ],
                                    "lastMessagePreview": {
                                        "id": "bg-2",
                                        "createdDateTime": "2026-11-20T03:02:00Z",
                                        "isDeleted": false,
                                        "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                        "body": { "contentType": "html", "content": preview_body }
                                    }
                                },
                                {
                                    "id": "c-bg-2",
                                    "chatType": "oneOnOne",
                                    "lastUpdatedDateTime": "2026-11-20T02:00:00Z",
                                    "members": [
                                        { "userId": "u1", "displayName": "Me" },
                                        { "userId": "u3", "displayName": "Bob" }
                                    ],
                                    "lastMessagePreview": {
                                        "id": "b2-1",
                                        "createdDateTime": "2026-11-20T02:00:00Z",
                                        "isDeleted": false,
                                        "from": { "user": { "id": "u3", "displayName": "Bob" } },
                                        "body": { "contentType": "html", "content": "<p>b2-initial</p>" }
                                    }
                                }
                            ]
                        }))
                }),
            )
            .route(
                "/v1.0/$batch",
                post(move |Json(req): Json<serde_json::Value>| {
                    let st = Arc::clone(&st_batch);
                    async move {
                        let s = st.load(Ordering::SeqCst);
                        let mut responses = Vec::new();
                        for sub in req["requests"].as_array().cloned().unwrap_or_default() {
                            let id = sub["id"].as_str().unwrap_or("0").to_string();
                            let url = sub["url"].as_str().unwrap_or("");
                            // Channel listings are requested from `/allChannels`.
                            if url.contains("/teams/t1/channels") || url.contains("/teams/t1/allChannels") {
                                responses.push(serde_json::json!({
                                    "id": id,
                                    "status": 200,
                                    "body": {
                                        "value": [{
                                            "id": "ch1",
                                            "displayName": "General",
                                            "membershipType": "standard"
                                        }]
                                    }
                                }));
                            } else if url.contains("/chats/c-bg-1/messages") {
                                let bg2_body = if s >= 1 {
                                    "<p>bg-last-EDITED</p>"
                                } else {
                                    "<p>bg-last-original</p>"
                                };
                                let bg1_body = if s >= 1 {
                                    "<p>bg-older-EDITED</p>"
                                } else {
                                    "<p>bg-older-original</p>"
                                };
                                responses.push(serde_json::json!({
                                    "id": id,
                                    "status": 200,
                                    "body": {
                                        "value": [
                                            {
                                                "id": "bg-2",
                                                "createdDateTime": "2026-11-20T03:02:00Z",
                                                "lastModifiedDateTime": if s >= 1 { serde_json::json!("2026-11-20T03:05:00Z") } else { serde_json::Value::Null },
                                                "messageType": "message",
                                                "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                                "body": { "contentType": "html", "content": bg2_body }
                                            },
                                            {
                                                "id": "bg-1",
                                                "createdDateTime": "2026-11-20T03:01:00Z",
                                                "lastModifiedDateTime": if s >= 1 { serde_json::json!("2026-11-20T03:05:00Z") } else { serde_json::Value::Null },
                                                "messageType": "message",
                                                "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                                "body": { "contentType": "html", "content": bg1_body }
                                            }
                                        ]
                                    }
                                }));
                            } else {
                                responses.push(serde_json::json!({
                                    "id": id,
                                    "status": 200,
                                    "body": {
                                        "value": [{
                                            "id": "b2-1",
                                            "createdDateTime": "2026-11-20T02:00:00Z",
                                            "messageType": "message",
                                            "from": { "user": { "id": "u3", "displayName": "Bob" } },
                                            "body": { "contentType": "html", "content": "<p>b2-initial</p>" }
                                        }]
                                    }
                                }));
                            }
                        }
                        Json(serde_json::json!({ "responses": responses }))
                    }
                }),
            )
            .route(
                "/v1.0/chats/c-rx/messages",
                get(move || {
                    let st = Arc::clone(&st_active);
                    async move {
                        let s = st.load(Ordering::SeqCst);
                        let reactions = match s {
                            0 => vec![serde_json::json!({
                                "reactionType": "like",
                                "user": { "user": { "id": "u2", "displayName": "Alice" } }
                            })],
                            _ => vec![
                                serde_json::json!({
                                    "reactionType": "like",
                                    "user": { "user": { "id": "u2", "displayName": "Alice" } }
                                }),
                                serde_json::json!({
                                    "reactionType": "like",
                                    "user": { "user": { "id": "u3", "displayName": "Bob" } }
                                }),
                            ],
                        };
                        Json(serde_json::json!({
                            "value": [{
                                "id": "rx-1",
                                "createdDateTime": "2026-11-20T04:00:00Z",
                                "messageType": "message",
                                "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                "body": { "contentType": "html", "content": "<p>react target</p>" },
                                "reactions": reactions
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "t1", "displayName": "Eng" }]
                    }))
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch1/messages/delta",
                get(move || {
                    let st = Arc::clone(&st_delta);
                    let delta_base = delta_base.clone();
                    async move {
                        let s = st.load(Ordering::SeqCst);
                        let value = match s {
                            0 => vec![serde_json::json!({
                                "id": "cm-1",
                                "createdDateTime": "2026-11-20T05:00:00Z",
                                "messageType": "message",
                                "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                "body": { "contentType": "html", "content": "<p>chan post</p>" },
                                "reactions": [{
                                    "reactionType": "like",
                                    "user": { "user": { "id": "u2", "displayName": "Alice" } }
                                }]
                            })],
                            1 => vec![serde_json::json!({
                                "id": "cm-1",
                                "createdDateTime": "2026-11-20T05:00:00Z",
                                "messageType": "message",
                                "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                "body": { "contentType": "html", "content": "<p>chan post</p>" },
                                "reactions": [
                                    { "reactionType": "like", "user": { "user": { "id": "u2", "displayName": "Alice" } } },
                                    { "reactionType": "like", "user": { "user": { "id": "u3", "displayName": "Bob" } } }
                                ]
                            })],
                            _ => vec![
                                serde_json::json!({
                                    "id": "cm-1",
                                    "@removed": { "reason": "deleted" }
                                }),
                                serde_json::json!({
                                    "id": "cm-2",
                                    "createdDateTime": "2026-11-20T05:10:00Z",
                                    "messageType": "message",
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "html", "content": "<p>@Me ping</p>" },
                                    "mentions": [{
                                        "id": 0,
                                        "mentionText": "Me",
                                        "mentioned": { "user": { "id": "u1", "displayName": "Me" } }
                                    }]
                                }),
                            ],
                        };
                        Json(serde_json::json!({
                            "value": value,
                            "@odata.deltaLink": format!("{delta_base}?s={s}")
                        }))
                    }
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Me".to_string()),
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1");
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        // 1) Initial tier-2 poll seeds c-bg-1 & c-bg-2.
        let mut chat_cache = HashMap::new();
        let mut seeded = false;
        let mut cursor_init = false;
        let mut rolling = 0usize;
        poll_chats_tier2_with_rolling_cache(
            &graph,
            &storage,
            &event_tx,
            &mut chat_cache,
            &mut seeded,
            &mut cursor_init,
            &mut rolling,
        )
        .await
        .unwrap();
        while event_rx.try_recv().is_ok() {}

        // 2) Stage 1: remote edits BOTH bg-1 and bg-2 while /me/chats stays identical.
        // Rolling batch must catch both edits, update preview, and emit notify_candidate: None!
        stage.store(1, Ordering::SeqCst);
        poll_chats_tier2_with_rolling_cache(
            &graph,
            &storage,
            &event_tx,
            &mut chat_cache,
            &mut seeded,
            &mut cursor_init,
            &mut rolling,
        )
        .await
        .unwrap();
        assert_eq!(
            storage.get_message("chat:c-bg-1", "bg-1").unwrap().unwrap().body_plain,
            "bg-older-EDITED"
        );
        assert_eq!(
            storage.get_message("chat:c-bg-1", "bg-2").unwrap().unwrap().body_plain,
            "bg-last-EDITED"
        );
        assert_eq!(
            storage
                .get_conversation("chat:c-bg-1")
                .unwrap()
                .unwrap()
                .last_msg_preview
                .as_deref(),
            Some("bg-last-EDITED")
        );
        let mut saw_bg_edit_event = false;
        let mut saw_conv_preview_event = false;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                CoreEvent::MessagesUpdated {
                    conv_key,
                    notify_candidate,
                    changed_msg_ids,
                    ..
                } if conv_key == "chat:c-bg-1" => {
                    assert!(
                        notify_candidate.is_none(),
                        "edits in rolling batch must not trigger notifications"
                    );
                    assert_eq!(
                        changed_msg_ids,
                        Some(vec!["bg-1".to_string(), "bg-2".to_string()])
                    );
                    saw_bg_edit_event = true;
                }
                CoreEvent::ConversationsUpdated(_) => saw_conv_preview_event = true,
                _ => {}
            }
        }
        assert!(saw_bg_edit_event);
        assert!(saw_conv_preview_event);

        // 3) Active chat reaction count 1 -> 2 must NOT collide in compute_messages_signature.
        stage.store(0, Ordering::SeqCst);
        let mut sigs = HashMap::new();
        let rx_focus = FocusTarget {
            target: Some(ConversationTarget::Chat {
                chat_id: "c-rx".to_string(),
            }),
            thread_root_id: None,
        };
        sync_active_focus_with_state(&rx_focus, &graph, &storage, &event_tx, &mut sigs, false)
            .await
            .unwrap();
        while event_rx.try_recv().is_ok() {}
        stage.store(1, Ordering::SeqCst);
        sync_active_focus_with_state(&rx_focus, &graph, &storage, &event_tx, &mut sigs, true)
            .await
            .unwrap();
        let rx_msg = storage.get_message("chat:c-rx", "rx-1").unwrap().unwrap();
        assert_eq!(rx_msg.reactions.len(), 1);
        assert_eq!(rx_msg.reactions[0].count, 2);

        // 4) Channel delta: stage 0 initial -> stage 1 reaction count 1->2 -> stage 2 @removed + @me mention via explicit RefreshConversations!
        stage.store(0, Ordering::SeqCst);
        let ch_focus = FocusTarget {
            target: Some(ConversationTarget::Channel {
                team_id: "t1".to_string(),
                channel_id: "ch1".to_string(),
            }),
            thread_root_id: None,
        };
        sync_active_focus_with_state(&ch_focus, &graph, &storage, &event_tx, &mut sigs, false)
            .await
            .unwrap();
        stage.store(1, Ordering::SeqCst);
        sync_active_focus_with_state(&ch_focus, &graph, &storage, &event_tx, &mut sigs, true)
            .await
            .unwrap();
        let cm1 = storage.get_message("chan:t1:ch1", "cm-1").unwrap().unwrap();
        assert_eq!(cm1.reactions[0].count, 2);

        // Now channel is inactive (active is chat:c-rx); explicit RefreshConversations syncs delta stage 2.
        stage.store(2, Ordering::SeqCst);
        // Reset delta_link to point at our mock server URL so refresh_channel_delta_once hits it.
        storage
            .save_sync_cursor(&SyncCursor {
                scope_key: "chan:t1:ch1".to_string(),
                delta_link: Some(format!("http://{addr}/v1.0/teams/t1/channels/ch1/messages/delta?s=1")),
                oldest_msg_ts: None,
                synced_at: "2026-11-20T05:00:00Z".to_string(),
            })
            .unwrap();
        refresh_all_conversations_with_cache(
            &graph,
            &storage,
            &event_tx,
            &mut chat_cache,
            &mut seeded,
            &mut cursor_init,
            &mut rolling,
            &HashSet::new(),
            None,
            Some("chat:c-rx"),
            None,
        )
        .await
        .unwrap();
        let cm1_after = storage.get_message("chan:t1:ch1", "cm-1").unwrap().unwrap();
        assert!(cm1_after.deleted, "@removed tombstone must mark cm-1 deleted");
        assert_eq!(cm1_after.author_name, "Alice", "tombstone must preserve author");
        let cm2 = storage.get_message("chan:t1:ch1", "cm-2").unwrap().unwrap();
        assert_eq!(cm2.mentions.len(), 1);
        let ch_conv = storage.get_conversation("chan:t1:ch1").unwrap().unwrap();
        assert_eq!(ch_conv.unread_count, 1, "inactive channel must record unread_count on new message");

        server.abort();
    }

    #[test]
    fn lane1_404_restricts_conversation_without_transient_retry_loop() {
        let focus = FocusTarget {
            target: Some(ConversationTarget::Chat {
                chat_id: "deleted-chat".to_string(),
            }),
            thread_root_id: None,
        };
        let shared = SyncSharedState::new();
        let mut restricted = HashSet::new();
        let mut err_backoff = ErrorBackoffState::new(1000);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let err = anyhow::anyhow!(
            "Graph API error (GET /chats/deleted-chat/messages): 404 Not Found"
        );
        handle_lane1_error(&err, &focus, &shared, &mut restricted, &mut err_backoff, &tx);

        assert!(restricted.contains("chat:deleted-chat"));
        assert!(!err_backoff.is_degraded());
        assert!(rx.try_recv().is_err(), "404 must not emit transient retry spinner");
    }

    #[tokio::test]
    async fn test_tier2_rotation_reaches_cached_chat_beyond_page1_even_under_20_plus_dirty_chats() {
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        use std::sync::{atomic::AtomicUsize, Mutex};

        let batch_sizes = Arc::new(Mutex::new(Vec::<usize>::new()));
        let bs_clone = Arc::clone(&batch_sizes);
        let tick_num = Arc::new(AtomicUsize::new(0));
        let tn_chats = Arc::clone(&tick_num);

        // Page 1 returns 24 chats: 22 continuously dirty chats + 2 clean chats.
        // Older cached chats `c-beyond-1` and `c-beyond-2` are NOT in `/me/chats` page 1!
        let app = Router::new()
            .route(
                "/v1.0/me/chats",
                get(move || {
                    let tn = Arc::clone(&tn_chats);
                    async move {
                        let t = tn.fetch_add(1, Ordering::SeqCst) + 1;
                        let mut chats = Vec::new();
                        for i in 0..22 {
                            let msg_id = format!("d-{i}-t{t}");
                            let ts = format!("2026-11-20T09:{:02}:{:02}Z", t, i);
                            chats.push(serde_json::json!({
                                "id": format!("c-dirty-{i:02}"),
                                "chatType": "oneOnOne",
                                "lastUpdatedDateTime": ts,
                                "members": [
                                    { "userId": "u1", "displayName": "Me" },
                                    { "userId": format!("u-d-{i}"), "displayName": format!("Dirty {i}") }
                                ],
                                "lastMessagePreview": {
                                    "id": msg_id,
                                    "createdDateTime": ts,
                                    "isDeleted": false,
                                    "from": { "user": { "id": format!("u-d-{i}"), "displayName": format!("Dirty {i}") } },
                                    "body": { "contentType": "html", "content": format!("<p>dirty {i} tick {t}</p>") }
                                }
                            }));
                        }
                        for i in 0..2 {
                            chats.push(serde_json::json!({
                                "id": format!("c-clean-{i:02}"),
                                "chatType": "oneOnOne",
                                "lastUpdatedDateTime": "2026-11-20T01:00:00Z",
                                "members": [
                                    { "userId": "u1", "displayName": "Me" },
                                    { "userId": format!("u-c-{i}"), "displayName": format!("Clean {i}") }
                                ],
                                "lastMessagePreview": {
                                    "id": format!("cl-{i}"),
                                    "createdDateTime": "2026-11-20T01:00:00Z",
                                    "isDeleted": false,
                                    "from": { "user": { "id": format!("u-c-{i}"), "displayName": format!("Clean {i}") } },
                                    "body": { "contentType": "html", "content": "<p>clean</p>" }
                                }
                            }));
                        }
                        Json(serde_json::json!({ "value": chats }))
                    }
                }),
            )
            .route(
                "/v1.0/$batch",
                post(move |Json(req): Json<serde_json::Value>| {
                    let bs = Arc::clone(&bs_clone);
                    async move {
                        let reqs = req["requests"].as_array().cloned().unwrap_or_default();
                        bs.lock().unwrap().push(reqs.len());
                        let mut responses = Vec::new();
                        for sub in reqs {
                            let id = sub["id"].as_str().unwrap_or("0").to_string();
                            let url = sub["url"].as_str().unwrap_or("");
                            if url.contains("/chats/c-beyond-2/messages") {
                                responses.push(serde_json::json!({
                                    "id": id,
                                    "status": 200,
                                    "body": {
                                        "value": [{
                                            "id": "old-msg-2",
                                            "createdDateTime": "2026-11-19T12:00:00Z",
                                            "lastModifiedDateTime": "2026-11-20T10:00:00Z",
                                            "messageType": "message",
                                            "from": { "user": { "id": "u-old", "displayName": "OldPeer" } },
                                            "body": { "contentType": "html", "content": "<p>EDITED-BEYOND-PAGE-1</p>" }
                                        }]
                                    }
                                }));
                            } else {
                                responses.push(serde_json::json!({
                                    "id": id,
                                    "status": 200,
                                    "body": { "value": [] }
                                }));
                            }
                        }
                        Json(serde_json::json!({ "responses": responses }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Me".to_string()),
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1");
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        // Pre-cache clean page-1 chats AND 2 older chats beyond page 1 in SQLite.
        let mut seed_convs = Vec::new();
        for i in 0..2 {
            let mut c = chat_summary(
                &format!("c-clean-{i:02}"),
                Some(&format!("cl-{i}")),
                Some("2026-11-20T01:00:00Z"),
            );
            c.title = format!("Clean {i}");
            c.last_msg_preview = Some("clean".to_string());
            seed_convs.push(c);
        }
        for i in 1..=2 {
            let mut c = chat_summary(
                &format!("c-beyond-{i}"),
                Some(&format!("old-msg-{i}")),
                Some("2026-11-19T12:00:00Z"),
            );
            c.title = format!("Beyond {i}");
            c.last_msg_preview = Some("original-beyond".to_string());
            seed_convs.push(c);
        }
        storage.upsert_conversations(&seed_convs).unwrap();
        let mut orig_msg = sample_msg("chat:c-beyond-2", "old-msg-2", "2026-11-19T12:00:00Z");
        orig_msg.body_plain = "original-beyond".to_string();
        orig_msg.body_html = "<p>original-beyond</p>".to_string();
        storage.upsert_messages(&[orig_msg]).unwrap();

        let mut chat_cache = HashMap::new();
        let mut seeded = false;
        let mut cursor_init = false;
        let mut rolling = 0usize;

        // Clean pool has 4 chats (c-clean-00, c-clean-01, c-beyond-1, c-beyond-2).
        // Under 22 continuously dirty chats, MIN_ROLLING_CHAT_SLOTS = 2 guarantees reaching
        // c-beyond-2 in 2 ticks while keeping every single $batch <= 20 sub-requests.
        for _ in 0..2 {
            poll_chats_tier2_with_rolling_cache(
                &graph,
                &storage,
                &event_tx,
                &mut chat_cache,
                &mut seeded,
                &mut cursor_init,
                &mut rolling,
            )
            .await
            .unwrap();
        }

        let recorded_sizes = batch_sizes.lock().unwrap().clone();
        assert_eq!(recorded_sizes.len(), 2, "must emit exactly 1 $batch per tick");
        assert!(
            recorded_sizes.iter().all(|&n| n <= MAX_TIER2_BATCH_SUBREQS),
            "every $batch must stay within 20-subrequest budget: {recorded_sizes:?}"
        );

        let updated_msg = storage
            .get_message("chat:c-beyond-2", "old-msg-2")
            .unwrap()
            .unwrap();
        assert_eq!(updated_msg.body_plain, "EDITED-BEYOND-PAGE-1");
        assert_eq!(
            storage
                .get_conversation("chat:c-beyond-2")
                .unwrap()
                .unwrap()
                .last_msg_preview
                .as_deref(),
            Some("EDITED-BEYOND-PAGE-1")
        );

        // Verify edit on beyond-page-1 chat emitted MessagesUpdated with notify_candidate == None.
        let mut saw_beyond_event = false;
        while let Ok(ev) = event_rx.try_recv() {
            if let CoreEvent::MessagesUpdated {
                conv_key,
                notify_candidate,
                changed_msg_ids,
                ..
            } = ev
            {
                if conv_key == "chat:c-beyond-2" {
                    assert!(notify_candidate.is_none());
                    assert_eq!(changed_msg_ids, Some(vec!["old-msg-2".to_string()]));
                    saw_beyond_event = true;
                }
            }
        }
        assert!(saw_beyond_event, "expected MessagesUpdated for beyond-page-1 chat");

        server.abort();
    }

    #[test]
    fn test_channel_rotation_pinned_recent_priority_and_no_starvation_beyond_20() {
        use crate::models::ChannelSummary;

        let make_channels = |count: usize| -> Vec<TeamSummary> {
            let channels = (0..count)
                .map(|i| ChannelSummary {
                    id: format!("ch-{i:02}"),
                    team_id: "t1".to_string(),
                    team_name: "Eng".to_string(),
                    display_name: format!("Chan {i:02}"),
                    description: None,
                    membership_type: "standard".to_string(),
                    web_url: None,
                })
                .collect();
            vec![TeamSummary {
                id: "t1".to_string(),
                display_name: "Eng".to_string(),
                description: None,
                channels,
            }]
        };

        let make_conv = |i: usize, pinned: bool, updated: Option<&str>| -> ConversationSummary {
            let target = ConversationTarget::Channel {
                team_id: "t1".to_string(),
                channel_id: format!("ch-{i:02}"),
            };
            ConversationSummary {
                conv_key: target.conv_key(),
                target,
                kind: ConversationKind::Channel,
                team_id: Some("t1".to_string()),
                team_name: Some("Eng".to_string()),
                title: format!("Chan {i:02}"),
                topic: None,
                unread_count: 0,
                last_read_at: None,
                last_msg_id: None,
                last_msg_author: None,
                last_msg_preview: None,
                last_updated_at: updated.map(str::to_string),
                pinned,
                peer_user_id: None,
                web_url: None,
            }
        };

        // 1) 5 channels -> all 5 covered in 1 tick; pinned & recent start first.
        let teams5 = make_channels(5);
        let convs5 = vec![
            make_conv(3, false, Some("2026-11-20T09:00:00Z")),
            make_conv(4, true, None),
        ];
        let mut cfg_pinned = HashSet::new();
        cfg_pinned.insert("chan:t1:ch-02".to_string());
        let restricted = HashSet::new();
        let cands5 = build_channel_poll_candidates(&teams5, &convs5, &cfg_pinned, None, &restricted);
        let mut rot5 = ChannelRotationState::default();
        let tick1 = rot5.select_next_batch(&cands5, BACKGROUND_CHANNEL_BATCH_SIZE);
        assert_eq!(tick1.len(), 5);
        // Pinned channels (ch-02 via config, ch-04 via DB) come first, then recent (ch-03).
        assert_eq!(tick1[0].channel_id, "ch-02");
        assert_eq!(tick1[1].channel_id, "ch-04");
        assert_eq!(tick1[2].channel_id, "ch-03");

        // 2) 10 channels -> all 10 covered in 2 ticks even if priorities change mid-round.
        let teams10 = make_channels(10);
        let mut rot10 = ChannelRotationState::default();
        let cands10_a =
            build_channel_poll_candidates(&teams10, &[], &HashSet::new(), None, &restricted);
        let batch1 = rot10.select_next_batch(&cands10_a, BACKGROUND_CHANNEL_BATCH_SIZE);
        assert_eq!(batch1.len(), 5);
        // Reprioritize already-polled channels as pinned + recent before tick 2.
        let convs10_reshuffled: Vec<ConversationSummary> = (0..5)
            .map(|i| make_conv(i, true, Some("2026-11-20T12:00:00Z")))
            .collect();
        let cands10_b = build_channel_poll_candidates(
            &teams10,
            &convs10_reshuffled,
            &HashSet::new(),
            None,
            &restricted,
        );
        let batch2 = rot10.select_next_batch(&cands10_b, BACKGROUND_CHANNEL_BATCH_SIZE);
        assert_eq!(batch2.len(), 5);
        let seen10: HashSet<String> = batch1
            .into_iter()
            .chain(batch2)
            .map(|c| c.conv_key)
            .collect();
        assert_eq!(seen10.len(), 10, "all 10 channels must be checked in 2 ticks without starvation");

        // 3) 22 channels (1 active excluded -> 21 inactive) -> 21st channel reached by tick 5, strict 5/tick cap.
        let teams22 = make_channels(22);
        let mut rot21 = ChannelRotationState::default();
        let mut seen21: HashSet<String> = HashSet::new();
        for tick in 0..5 {
            // Even with dynamic recency churn on top channels, all 21 must rotate without starvation.
            let churn: Vec<ConversationSummary> = (0..6)
                .map(|i| make_conv(i, i < 2, Some(&format!("2026-11-20T10:{:02}:00Z", tick))))
                .collect();
            let cands = build_channel_poll_candidates(
                &teams22,
                &churn,
                &HashSet::new(),
                Some("chan:t1:ch-00"),
                &restricted,
            );
            assert_eq!(cands.len(), 21);
            assert!(!cands.iter().any(|c| c.conv_key == "chan:t1:ch-00"));
            let batch = rot21.select_next_batch(&cands, BACKGROUND_CHANNEL_BATCH_SIZE);
            assert_eq!(batch.len(), BACKGROUND_CHANNEL_BATCH_SIZE);
            let uniq: HashSet<&str> = batch.iter().map(|c| c.conv_key.as_str()).collect();
            assert_eq!(uniq.len(), BACKGROUND_CHANNEL_BATCH_SIZE);
            for c in batch {
                seen21.insert(c.conv_key);
            }
        }
        assert_eq!(seen21.len(), 21, "all 21 inactive channels including #21 must be polled within 5 ticks");
    }

    #[test]
    fn notifying_activity_preserves_idle_until_the_lane_handles_it() {
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = mpsc::unbounded_channel();
        let controller = SyncController::from_channels(focus_tx, nudge_tx, false);
        let (_, before) = controller.shared.idle_snapshot(Instant::now());

        controller.notify_activity();

        assert!(matches!(nudge_rx.try_recv(), Ok(SyncCommand::NotifyUserActivity)));
        let (_, after) = controller.shared.idle_snapshot(Instant::now());
        assert_eq!(before, after);
    }

    #[test]
    fn channel_delta_cannot_apply_an_older_snapshot() {
        let storage = Storage::open_in_memory().unwrap();
        let key = "chan:t1:ch1";
        let old_cursor = SyncCursor {
            scope_key: key.to_string(),
            delta_link: Some("https://example.invalid/old".to_string()),
            oldest_msg_ts: None,
            synced_at: "2026-11-20T05:00:00Z".to_string(),
        };
        storage.save_sync_cursor(&old_cursor).unwrap();
        let new_cursor = SyncCursor {
            delta_link: Some("https://example.invalid/new".to_string()),
            ..old_cursor.clone()
        };
        let mut fresh = sample_msg(key, "edited", "2026-11-20T05:00:30Z");
        fresh.body_plain = "latest edit".to_string();
        assert!(storage
            .apply_channel_delta(Some(&old_cursor), &new_cursor, &[fresh], &HashSet::new())
            .unwrap()
            .is_some());

        let stale = sample_msg(key, "edited", "2026-11-20T05:00:30Z");
        assert!(storage
            .apply_channel_delta(Some(&old_cursor), &old_cursor, &[stale], &HashSet::new())
            .unwrap()
            .is_none());
        assert_eq!(storage.get_sync_cursor(key).unwrap(), Some(new_cursor));
        assert_eq!(storage.list_root_messages(key, 1, None).unwrap()[0].body_plain, "latest edit");
    }

    #[tokio::test]
    async fn test_background_channel_delta_410_recovery_drops_stale_cursor_and_gets_fresh_link() {
        use axum::{extract::RawQuery, http::StatusCode, routing::get, Json, Router};
        use std::sync::Mutex;

        let seen_delta_queries = Arc::new(Mutex::new(Vec::<String>::new()));
        let sq = Arc::clone(&seen_delta_queries);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let fresh_delta_link =
            format!("http://{addr}/v1.0/teams/t1/channels/ch1/messages/delta?state=fresh-2");
        let fresh_for_route = fresh_delta_link.clone();

        let app = Router::new()
            .route(
                "/v1.0/teams/t1/channels/ch1/messages/delta",
                get(move |RawQuery(q): RawQuery| {
                    let sq = Arc::clone(&sq);
                    let fresh_dl = fresh_for_route.clone();
                    async move {
                        let q_str = q.unwrap_or_default();
                        sq.lock().unwrap().push(q_str.clone());
                        if q_str.contains("state=stale") {
                            (
                                StatusCode::GONE,
                                Json(serde_json::json!({
                                    "error": { "code": "resyncRequired", "message": "Expired delta token" }
                                })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [
                                        {
                                            "id": "cm-1",
                                            "createdDateTime": "2026-11-20T05:00:00Z",
                                            "messageType": "message",
                                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                            "body": { "contentType": "html", "content": "<p>existing</p>" }
                                        },
                                        {
                                            "id": "cm-2",
                                            "createdDateTime": "2026-11-20T05:05:00Z",
                                            "messageType": "message",
                                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                            "body": { "contentType": "html", "content": "<p>after 410 fresh</p>" }
                                        }
                                    ],
                                    "@odata.deltaLink": fresh_dl
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch1/messages",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "cm-1",
                            "createdDateTime": "2026-11-20T05:00:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>existing</p>" }
                        }]
                    }))
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Me".to_string()),
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1");
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_teams(&[TeamSummary {
                id: "t1".to_string(),
                display_name: "Eng".to_string(),
                description: None,
                channels: vec![crate::models::ChannelSummary {
                    id: "ch1".to_string(),
                    team_id: "t1".to_string(),
                    team_name: "Eng".to_string(),
                    display_name: "General".to_string(),
                    description: None,
                    membership_type: "standard".to_string(),
                    web_url: None,
                }],
            }])
            .unwrap();
        storage
            .upsert_messages(&[sample_msg("chan:t1:ch1", "cm-1", "2026-11-20T05:00:00Z")])
            .unwrap();
        storage
            .save_sync_cursor(&SyncCursor {
                scope_key: "chan:t1:ch1".to_string(),
                delta_link: Some(format!(
                    "http://{addr}/v1.0/teams/t1/channels/ch1/messages/delta?state=stale"
                )),
                oldest_msg_ts: None,
                synced_at: "2026-11-20T05:00:00Z".to_string(),
            })
            .unwrap();

        let (event_tx, mut event_rx) = mpsc::unbounded_channel();

        // First call hits 410 -> MUST drop stale delta_link to None in SQLite!
        refresh_channel_delta_once(
            &graph,
            &storage,
            &event_tx,
            "t1",
            "ch1",
            "chan:t1:ch1",
            "Eng › #General",
            true,
            None,
        )
        .await
        .unwrap();
        assert!(
            storage
                .get_sync_cursor("chan:t1:ch1")
                .unwrap()
                .unwrap()
                .delta_link
                .is_none(),
            "410 recovery must drop stale delta_link instead of re-saving it"
        );

        // Second call starts fresh delta without stale token and saves the new deltaLink.
        let outcome2 = refresh_channel_delta_once(
            &graph,
            &storage,
            &event_tx,
            "t1",
            "ch1",
            "chan:t1:ch1",
            "Eng › #General",
            true,
            None,
        )
        .await
        .unwrap();
        assert!(outcome2.any_summary_changed);
        assert_eq!(
            storage
                .get_sync_cursor("chan:t1:ch1")
                .unwrap()
                .unwrap()
                .delta_link
                .as_deref(),
            Some(fresh_delta_link.as_str())
        );
        // Rebuilding the cursor can find uncached history without producing unread or a toast.
        assert_eq!(
            storage
                .get_conversation("chan:t1:ch1")
                .unwrap()
                .unwrap()
                .unread_count,
            0
        );
        while let Ok(event) = event_rx.try_recv() {
            if let CoreEvent::MessagesUpdated { notify_candidate, .. } = event {
                assert!(notify_candidate.is_none());
            }
        }
        let queries = seen_delta_queries.lock().unwrap().clone();
        assert_eq!(queries.len(), 2);
        assert!(queries[0].contains("state=stale"));
        assert!(!queries[1].contains("state=stale"));

        server.abort();
    }

    #[tokio::test]
    async fn test_background_channel_delta_unread_notifications_edits_reactions_deletes_and_search() {
        use axum::{routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let stage = Arc::new(AtomicUsize::new(0));
        let st_route = Arc::clone(&stage);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let delta_base = format!("http://{addr}/v1.0/teams/t1/channels/ch1/messages/delta");

        let app = Router::new().route(
            "/v1.0/teams/t1/channels/ch1/messages/delta",
            get(move || {
                let st = Arc::clone(&st_route);
                let base = delta_base.clone();
                async move {
                    let s = st.load(Ordering::SeqCst);
                    let value = match s {
                        0 => vec![serde_json::json!({
                            "id": "cm-0",
                            "createdDateTime": "2026-11-20T04:00:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>HISTORICAL-SEED</p>" }
                        })],
                        1 | 2 => vec![serde_json::json!({
                            "id": "cm-1",
                            "createdDateTime": "2026-11-20T05:00:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>FRESH-ALPHA-KEYWORD</p>" }
                        })],
                        3 => vec![serde_json::json!({
                            "id": "cm-1",
                            "createdDateTime": "2026-11-20T05:00:00Z",
                            "lastModifiedDateTime": "2026-11-20T05:01:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>EDITED-BETA-KEYWORD</p>" }
                        })],
                        4 => vec![serde_json::json!({
                            "id": "cm-1",
                            "createdDateTime": "2026-11-20T05:00:00Z",
                            "lastModifiedDateTime": "2026-11-20T05:01:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>EDITED-BETA-KEYWORD</p>" },
                            "reactions": [{
                                "reactionType": "heart",
                                "user": { "user": { "id": "u3", "displayName": "Bob" } }
                            }]
                        })],
                        6 => vec![serde_json::json!({
                            "id": "uncached-old-edit",
                            "createdDateTime": "2026-01-01T00:00:00Z",
                            "lastModifiedDateTime": "2026-11-20T05:01:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>UNCACHED-HISTORICAL-EDIT</p>" }
                        })],
                        7 => vec![serde_json::json!({
                            "id": "delayed-original",
                            "createdDateTime": "2026-01-02T00:00:00Z",
                            "lastModifiedDateTime": "2026-01-02T00:00:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>DELAYED-ORIGINAL</p>" }
                        })],
                        _ => vec![serde_json::json!({
                            "id": "cm-1",
                            "@removed": { "reason": "deleted" }
                        })],
                    };
                    Json(serde_json::json!({
                        "value": value,
                        "@odata.deltaLink": format!("{base}?s={s}")
                    }))
                }
            }),
        );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Me".to_string()),
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1");
        let storage = Storage::open_in_memory().unwrap();
        storage
            .upsert_teams(&[TeamSummary {
                id: "t1".to_string(),
                display_name: "Eng".to_string(),
                description: None,
                channels: vec![crate::models::ChannelSummary {
                    id: "ch1".to_string(),
                    team_id: "t1".to_string(),
                    team_name: "Eng".to_string(),
                    display_name: "General".to_string(),
                    description: None,
                    membership_type: "standard".to_string(),
                    web_url: None,
                }],
            }])
            .unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut rotation = ChannelRotationState::default();
        let mut restricted = HashSet::new();
        let pinned = HashSet::new();

        // Stage 0: First sync -> historical message, 0 unread, 0 notification.
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            Some("chat:c-open"),
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        assert_eq!(
            storage.get_conversation("chan:t1:ch1").unwrap().unwrap().unread_count,
            0
        );
        while let Ok(ev) = event_rx.try_recv() {
            if let CoreEvent::MessagesUpdated { notify_candidate, .. } = ev {
                assert!(notify_candidate.is_none(), "initial sync must not notify");
            }
        }

        // Stage 1: New message -> unread = 1, notify = Some, ConversationsUpdated emitted, searchable.
        stage.store(1, Ordering::SeqCst);
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            Some("chat:c-open"),
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        assert_eq!(
            storage.get_conversation("chan:t1:ch1").unwrap().unwrap().unread_count,
            1
        );
        let mut saw_notify = false;
        let mut saw_sidebar_convs = false;
        while let Ok(ev) = event_rx.try_recv() {
            match ev {
                CoreEvent::MessagesUpdated { notify_candidate, .. } => {
                    saw_notify = notify_candidate.is_some();
                }
                CoreEvent::ConversationsUpdated(convs) => {
                    saw_sidebar_convs = convs
                        .iter()
                        .any(|c| c.conv_key == "chan:t1:ch1" && c.unread_count == 1);
                }
                _ => {}
            }
        }
        assert!(saw_notify);
        assert!(saw_sidebar_convs);
        assert_eq!(storage.search_messages("FRESH-ALPHA-KEYWORD", None, 10).unwrap().len(), 1);

        // Stage 2: Re-delivery of same message -> unread stays 1, 0 notifications.
        stage.store(2, Ordering::SeqCst);
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            Some("chat:c-open"),
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        assert_eq!(
            storage.get_conversation("chan:t1:ch1").unwrap().unwrap().unread_count,
            1
        );
        assert!(event_rx.try_recv().is_err(), "re-delivery should emit zero events");

        // Stage 3: Same-ID edit -> unread stays 1, notify = None, search updated.
        stage.store(3, Ordering::SeqCst);
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            Some("chat:c-open"),
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        assert_eq!(
            storage.get_conversation("chan:t1:ch1").unwrap().unwrap().unread_count,
            1
        );
        while let Ok(ev) = event_rx.try_recv() {
            if let CoreEvent::MessagesUpdated { notify_candidate, .. } = ev {
                assert!(notify_candidate.is_none(), "edit must not notify");
            }
        }
        assert_eq!(storage.search_messages("FRESH-ALPHA-KEYWORD", None, 10).unwrap().len(), 0);
        assert_eq!(storage.search_messages("EDITED-BETA-KEYWORD", None, 10).unwrap().len(), 1);

        // Stage 4: Reaction -> unread stays 1, notify = None, reaction stored.
        stage.store(4, Ordering::SeqCst);
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            Some("chat:c-open"),
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        assert_eq!(
            storage.get_conversation("chan:t1:ch1").unwrap().unwrap().unread_count,
            1
        );
        while let Ok(ev) = event_rx.try_recv() {
            if let CoreEvent::MessagesUpdated { notify_candidate, .. } = ev {
                assert!(notify_candidate.is_none(), "reaction must not notify");
            }
        }
        assert_eq!(
            storage.get_message("chan:t1:ch1", "cm-1").unwrap().unwrap().reactions.len(),
            1
        );

        // Stage 5: Deletion -> unread stays 1, notify = None, removed from search, preview updated.
        stage.store(5, Ordering::SeqCst);
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            Some("chat:c-open"),
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        assert_eq!(
            storage.get_conversation("chan:t1:ch1").unwrap().unwrap().unread_count,
            1
        );
        assert_eq!(storage.search_messages("EDITED-BETA-KEYWORD", None, 10).unwrap().len(), 0);
        assert!(storage.get_message("chan:t1:ch1", "cm-1").unwrap().unwrap().deleted);
        assert_eq!(
            storage
                .get_conversation("chan:t1:ch1")
                .unwrap()
                .unwrap()
                .last_msg_preview
                .as_deref(),
            Some("(deleted message)")
        );

        stage.store(6, Ordering::SeqCst);
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            Some("chat:c-open"),
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        let summary = storage.get_conversation("chan:t1:ch1").unwrap().unwrap();
        assert_eq!(summary.unread_count, 1);
        assert_eq!(
            storage.search_messages("UNCACHED-HISTORICAL-EDIT", None, 10).unwrap().len(),
            1
        );
        while let Ok(event) = event_rx.try_recv() {
            if let CoreEvent::MessagesUpdated { notify_candidate, .. } = event {
                assert!(notify_candidate.is_none());
            }
        }

        stage.store(7, Ordering::SeqCst);
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            Some("chat:c-open"),
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        let summary = storage.get_conversation("chan:t1:ch1").unwrap().unwrap();
        assert_eq!(summary.unread_count, 2);
        assert!(matches!(event_rx.try_recv(), Ok(CoreEvent::MessagesUpdated {
            notify_candidate: Some(candidate), ..
        }) if candidate.preview == "DELAYED-ORIGINAL"));

        server.abort();
    }

    #[tokio::test]
    async fn test_window_focus_reading_vs_unfocused_throttle_and_non_destructive_resume() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        // 1) Pure state-machine checks: reading & no-focus-reporting stay at 1x even after 10 min;
        //    explicitly unfocused window throttles to 4x immediately and 10x at 600s.
        assert_eq!(
            effective_active_idle_multiplier(WindowFocusState::Focused, Duration::from_secs(700)),
            1
        );
        assert_eq!(
            effective_active_idle_multiplier(WindowFocusState::Unknown, Duration::from_secs(700)),
            1
        );
        assert_eq!(
            effective_active_idle_multiplier(WindowFocusState::Unfocused, Duration::from_secs(5)),
            4
        );
        assert_eq!(
            effective_active_idle_multiplier(WindowFocusState::Unfocused, Duration::from_secs(600)),
            10
        );

        // 2) Live engine check: focus-gained resume respects 403 circuit breaker and throttles rapid flap.
        let c403_hits = Arc::new(AtomicUsize::new(0));
        let cok_hits = Arc::new(AtomicUsize::new(0));
        let h403 = Arc::clone(&c403_hits);
        let hok = Arc::clone(&cok_hits);

        let app = Router::new()
            .route(
                "/v1.0/chats/c403/messages",
                get(move || {
                    let h = Arc::clone(&h403);
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::FORBIDDEN,
                            Json(serde_json::json!({ "error": { "code": "Forbidden", "message": "Denied" } })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/chats/c-ok/messages",
                get(move || {
                    let h = Arc::clone(&hok);
                    async move {
                        h.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [{
                                    "id": "m1",
                                    "createdDateTime": "2026-11-20T05:00:00Z",
                                    "messageType": "message",
                                    "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                    "body": { "contentType": "html", "content": "<p>ok</p>" }
                                }]
                            })),
                        )
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            active_poll_ms: 2_500,
            chat_poll_ms: 60_000,
            channel_poll_ms: 60_000,
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let ctrl = spawn_sync_engine(cfg, graph, storage, event_tx);

        // 403 room: FocusLost -> FocusGained must NOT clear restricted_convs.
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c403".to_string(),
            }),
            None,
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(c403_hits.load(Ordering::SeqCst), 1);
        ctrl.set_window_focus(false);
        tokio::time::sleep(Duration::from_millis(30)).await;
        ctrl.set_window_focus(true);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            c403_hits.load(Ordering::SeqCst),
            1,
            "automatic focus resume must not clear 403 restriction"
        );

        // Normal room: immediate focus sync -> rapid FocusLost/FocusGained flap within resume_min_gap is throttled.
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c-ok".to_string(),
            }),
            None,
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        let baseline_hits = cok_hits.load(Ordering::SeqCst);
        assert_eq!(baseline_hits, 1);
        ctrl.set_window_focus(false);
        ctrl.set_window_focus(true);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            cok_hits.load(Ordering::SeqCst),
            baseline_hits,
            "rapid focus return right after sync must be throttled"
        );

        ctrl.request(SyncCommand::Shutdown);
        server.abort();
    }

    #[tokio::test]
    async fn test_no_late_stale_focus_results_on_rapid_focus_switch() {
        use axum::{routing::get, Json, Router};

        let app = Router::new()
            .route(
                "/v1.0/chats/c-slow/messages",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    Json(serde_json::json!({
                        "value": [{
                            "id": "slow-1",
                            "createdDateTime": "2026-11-20T05:00:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "html", "content": "<p>STALE-SLOW</p>" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/chats/c-fast/messages",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "fast-1",
                            "createdDateTime": "2026-11-20T05:01:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u3", "displayName": "Bob" } },
                            "body": { "contentType": "html", "content": "<p>CURRENT-FAST</p>" }
                        }]
                    }))
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            active_poll_ms: 10_000,
            chat_poll_ms: 60_000,
            channel_poll_ms: 60_000,
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let ctrl = spawn_sync_engine(cfg, graph, storage, event_tx);

        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c-slow".to_string(),
            }),
            None,
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
        ctrl.set_focus(
            Some(ConversationTarget::Chat {
                chat_id: "c-fast".to_string(),
            }),
            None,
        );

        let mut saw_fast = false;
        let mut saw_slow = false;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(650);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
                Ok(Some(CoreEvent::MessagesUpdated { conv_key, .. })) => {
                    if conv_key == "chat:c-fast" {
                        saw_fast = true;
                    } else if conv_key == "chat:c-slow" {
                        saw_slow = true;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {}
            }
        }
        assert!(saw_fast, "expected fast current focus event");
        assert!(!saw_slow, "stale slow focus response must never emit MessagesUpdated");

        ctrl.request(SyncCommand::Shutdown);
        server.abort();
    }

    fn regression_graph(addr: std::net::SocketAddr) -> GraphClient {
        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Me".to_string()),
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1")
    }

    // TEAMS-10: a chat whose `$batch` sub-response is throttled must stay dirty, so the next
    // tick fetches it again; the other chats of the same batch are processed as usual.
    #[tokio::test]
    async fn regression_teams_10_throttled_batch_subresponse_stays_dirty() {
        use axum::{
            routing::{get, post},
            Json, Router,
        };
        use std::sync::{atomic::AtomicUsize, Mutex};

        let batch_calls = Arc::new(AtomicUsize::new(0));
        let bc = Arc::clone(&batch_calls);
        let batch_urls = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let bu = Arc::clone(&batch_urls);

        let chat = |id: &str| {
            serde_json::json!({
                "id": id,
                "chatType": "oneOnOne",
                "lastUpdatedDateTime": "2026-11-20T09:00:00Z",
                "members": [
                    { "userId": "u1", "displayName": "Me" },
                    { "userId": format!("peer-{id}"), "displayName": format!("Peer {id}") }
                ],
                "lastMessagePreview": {
                    "id": format!("{id}-m1"),
                    "createdDateTime": "2026-11-20T09:00:00Z",
                    "isDeleted": false,
                    "from": { "user": { "id": format!("peer-{id}"), "displayName": format!("Peer {id}") } },
                    "body": { "contentType": "html", "content": "<p>hello</p>" }
                }
            })
        };
        let chats = serde_json::json!({ "value": [chat("c-ok"), chat("c-throttled")] });

        let app = Router::new()
            .route(
                "/v1.0/me/chats",
                get(move || {
                    let chats = chats.clone();
                    async move { Json(chats) }
                }),
            )
            .route(
                "/v1.0/$batch",
                post(move |Json(req): Json<serde_json::Value>| {
                    let bc = Arc::clone(&bc);
                    let bu = Arc::clone(&bu);
                    async move {
                        let call = bc.fetch_add(1, Ordering::SeqCst) + 1;
                        let reqs = req["requests"].as_array().cloned().unwrap_or_default();
                        let mut urls = Vec::new();
                        let mut responses = Vec::new();
                        for sub in reqs {
                            let id = sub["id"].as_str().unwrap_or("0").to_string();
                            let url = sub["url"].as_str().unwrap_or("").to_string();
                            let chat_id = if url.contains("/chats/c-throttled/") {
                                "c-throttled"
                            } else {
                                "c-ok"
                            };
                            if chat_id == "c-throttled" && call == 1 {
                                responses.push(serde_json::json!({
                                    "id": id,
                                    "status": 429,
                                    "headers": { "Retry-After": "1" },
                                    "body": { "error": { "code": "TooManyRequests", "message": "throttled" } }
                                }));
                            } else {
                                responses.push(serde_json::json!({
                                    "id": id,
                                    "status": 200,
                                    "body": {
                                        "value": [{
                                            "id": format!("{chat_id}-m1"),
                                            "createdDateTime": "2026-11-20T09:00:00Z",
                                            "messageType": "message",
                                            "from": { "user": { "id": format!("peer-{chat_id}"), "displayName": "Peer" } },
                                            "body": { "contentType": "html", "content": "<p>hello</p>" }
                                        }]
                                    }
                                }));
                            }
                            urls.push(url);
                        }
                        bu.lock().unwrap().push(urls);
                        Json(serde_json::json!({ "responses": responses }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let graph = regression_graph(addr);
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let mut chat_cache = HashMap::new();
        let mut seeded = false;
        let mut cursor_init = false;
        let mut rolling = 0usize;

        // Tick 1: c-ok is stored and recorded clean; c-throttled is neither.
        poll_chats_tier2_with_rolling_cache(
            &graph,
            &storage,
            &event_tx,
            &mut chat_cache,
            &mut seeded,
            &mut cursor_init,
            &mut rolling,
        )
        .await
        .unwrap();
        assert!(storage.get_message("chat:c-ok", "c-ok-m1").unwrap().is_some());
        assert!(storage.get_message("chat:c-throttled", "c-throttled-m1").unwrap().is_none());
        assert_eq!(
            chat_cache.get("chat:c-ok").and_then(|e| e.last_msg_id.as_deref()),
            Some("c-ok-m1")
        );
        assert_ne!(
            chat_cache.get("chat:c-throttled").and_then(|e| e.last_msg_id.as_deref()),
            Some("c-throttled-m1"),
            "a throttled chat must not be recorded as clean"
        );

        // Tick 2: the throttled chat is asked for again and its message arrives.
        poll_chats_tier2_with_rolling_cache(
            &graph,
            &storage,
            &event_tx,
            &mut chat_cache,
            &mut seeded,
            &mut cursor_init,
            &mut rolling,
        )
        .await
        .unwrap();
        let urls = batch_urls.lock().unwrap().clone();
        assert_eq!(urls.len(), 2, "one $batch per tick, no extra request in the same tick: {urls:?}");
        assert!(
            urls[1].iter().any(|u| u.contains("/chats/c-throttled/messages")),
            "tick 2 must retry the throttled chat: {urls:?}"
        );
        assert!(storage.get_message("chat:c-throttled", "c-throttled-m1").unwrap().is_some());
        assert_eq!(
            chat_cache.get("chat:c-throttled").and_then(|e| e.last_msg_id.as_deref()),
            Some("c-throttled-m1")
        );

        server.abort();
    }

    // TEAMS-20: a deleted channel (404 on delta) leaves the rotation without aborting the
    // batch or raising a backoff; the channels after it are still polled in the same tick.
    #[tokio::test]
    async fn regression_teams_20_deleted_channel_does_not_abort_rotation() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::AtomicUsize;

        let gone_hits = Arc::new(AtomicUsize::new(0));
        let live_hits = Arc::new(AtomicUsize::new(0));
        let gh = Arc::clone(&gone_hits);
        let lh = Arc::clone(&live_hits);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let live_delta_link =
            format!("http://{addr}/v1.0/teams/t1/channels/ch-live/messages/delta?state=next");

        let gone_route = move || {
            let gh = Arc::clone(&gh);
            async move {
                gh.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": { "code": "NotFound", "message": "The channel was deleted." }
                    })),
                )
            }
        };
        let app = Router::new()
            .route("/v1.0/teams/t1/channels/ch-gone/messages/delta", get(gone_route.clone()))
            .route("/v1.0/teams/t1/channels/ch-gone/messages", get(gone_route))
            .route(
                "/v1.0/teams/t1/channels/ch-live/messages/delta",
                get(move || {
                    let lh = Arc::clone(&lh);
                    let link = live_delta_link.clone();
                    async move {
                        lh.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "value": [{
                                "id": "live-1",
                                "createdDateTime": "2026-11-20T05:00:00Z",
                                "messageType": "message",
                                "from": { "user": { "id": "u2", "displayName": "Alice" } },
                                "body": { "contentType": "html", "content": "<p>still here</p>" }
                            }],
                            "@odata.deltaLink": link
                        }))
                    }
                }),
            );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let graph = regression_graph(addr);
        let storage = Storage::open_in_memory().unwrap();
        let channel = |id: &str, name: &str| crate::models::ChannelSummary {
            id: id.to_string(),
            team_id: "t1".to_string(),
            team_name: "Eng".to_string(),
            display_name: name.to_string(),
            description: None,
            membership_type: "standard".to_string(),
            web_url: None,
        };
        storage
            .upsert_teams(&[TeamSummary {
                id: "t1".to_string(),
                display_name: "Eng".to_string(),
                description: None,
                channels: vec![channel("ch-gone", "Deleted"), channel("ch-live", "General")],
            }])
            .unwrap();

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let pinned = HashSet::new();
        let mut rotation = ChannelRotationState::default();
        let mut restricted = HashSet::new();

        // Before the fix the 404 was returned as the tick's error (and counted as a transient
        // failure by the caller) and the rest of the batch was not polled.
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            None,
            &mut rotation,
            &mut restricted,
        )
        .await
        .expect("a deleted channel must not fail the tick");
        assert_eq!(gone_hits.load(Ordering::SeqCst), 1);
        assert_eq!(live_hits.load(Ordering::SeqCst), 1, "the other channel is polled in the same tick");
        assert!(storage.get_message("chan:t1:ch-live", "live-1").unwrap().is_some());
        assert!(restricted.contains("chan:t1:ch-gone"));
        assert!(!restricted.contains("chan:t1:ch-live"));

        // The deleted channel is out of the rotation on the following ticks.
        poll_channels_tier3_rotating(
            &graph,
            &storage,
            &event_tx,
            None,
            &pinned,
            None,
            &mut rotation,
            &mut restricted,
        )
        .await
        .unwrap();
        assert_eq!(gone_hits.load(Ordering::SeqCst), 1);
        assert_eq!(live_hits.load(Ordering::SeqCst), 2);

        server.abort();
    }

    #[tokio::test]
    async fn refresh_all_conversations_overlaps_channel_deltas_and_presence_and_emits_teams_early() {
        use axum::{routing::{get, post}, Json, Router};
        use tokio::sync::Barrier;

        let barrier = Arc::new(Barrier::new(3));
        let b_ch1 = Arc::clone(&barrier);
        let b_ch2 = Arc::clone(&barrier);
        let b_pres = Arc::clone(&barrier);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let ch1_delta = format!("{base}/teams/t1/channels/ch1/messages/delta?state=ok");
        let ch2_delta = format!("{base}/teams/t1/channels/ch2/messages/delta?state=ok");

        let app = Router::new()
            .route(
                "/v1.0/me/chats",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "c-dm",
                            "chatType": "oneOnOne",
                            "members": [
                                { "userId": "me", "displayName": "Me" },
                                { "userId": "peer-1", "displayName": "Alice" }
                            ]
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "t1", "displayName": "Eng" }]
                    }))
                }),
            )
            .route(
                "/v1.0/$batch",
                post(|| async {
                    Json(serde_json::json!({
                        "responses": [{
                            "id": "0",
                            "status": 200,
                            "body": {
                                "value": [
                                    { "id": "ch1", "displayName": "General" },
                                    { "id": "ch2", "displayName": "Alerts" }
                                ]
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch1/messages/delta",
                get(move || {
                    let barrier = Arc::clone(&b_ch1);
                    let link = ch1_delta.clone();
                    async move {
                        barrier.wait().await;
                        Json(serde_json::json!({
                            "value": [{
                                "id": "m-ch1",
                                "createdDateTime": "2026-11-20T06:00:00Z",
                                "messageType": "message",
                                "from": { "user": { "id": "peer-1", "displayName": "Alice" } },
                                "body": { "contentType": "text", "content": "hello ch1" }
                            }],
                            "@odata.deltaLink": link
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch2/messages/delta",
                get(move || {
                    let barrier = Arc::clone(&b_ch2);
                    let link = ch2_delta.clone();
                    async move {
                        barrier.wait().await;
                        Json(serde_json::json!({
                            "value": [{
                                "id": "m-ch2",
                                "createdDateTime": "2026-11-20T06:01:00Z",
                                "messageType": "message",
                                "from": { "user": { "id": "peer-1", "displayName": "Alice" } },
                                "body": { "contentType": "text", "content": "hello ch2" }
                            }],
                            "@odata.deltaLink": link
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/communications/getPresencesByUserId",
                post(move || {
                    let barrier = Arc::clone(&b_pres);
                    async move {
                        barrier.wait().await;
                        Json(serde_json::json!({
                            "value": [{
                                "id": "peer-1",
                                "availability": "Available",
                                "activity": "Available"
                            }]
                        }))
                    }
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let graph = regression_graph(addr);
        let storage = Storage::open_in_memory().unwrap();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let mut chat_cache = ChatPollCache::new();
        let mut chat_cache_seeded = false;
        let mut chats_cursor_initialized = false;
        let mut rolling_cursor = 0usize;
        let pinned = HashSet::new();
        let mut presence_cache = PresencePollCache::new();

        tokio::time::timeout(
            Duration::from_secs(3),
            refresh_all_conversations_with_cache(
                &graph,
                &storage,
                &event_tx,
                &mut chat_cache,
                &mut chat_cache_seeded,
                &mut chats_cursor_initialized,
                &mut rolling_cursor,
                &pinned,
                None,
                None,
                Some(&mut presence_cache),
            ),
        )
        .await
        .expect("channel deltas and presence polling must execute concurrently")
        .unwrap();

        let mut events = Vec::new();
        while let Ok(ev) = event_rx.try_recv() {
            events.push(ev);
        }
        let teams_pos = events
            .iter()
            .position(|e| matches!(e, CoreEvent::TeamsUpdated(_)))
            .expect("TeamsUpdated event must be emitted");
        let last_convs_pos = events
            .iter()
            .rposition(|e| matches!(e, CoreEvent::ConversationsUpdated(_)))
            .expect("ConversationsUpdated event must be emitted");
        assert!(
            teams_pos < last_convs_pos,
            "TeamsUpdated should be emitted before final post-delta ConversationsUpdated"
        );
        assert!(events.iter().any(|e| matches!(e, CoreEvent::PresenceUpdated(_))));
        assert!(storage.get_message("chan:t1:ch1", "m-ch1").unwrap().is_some());
        assert!(storage.get_message("chan:t1:ch2", "m-ch2").unwrap().is_some());

        server.abort();
    }

    #[tokio::test]
    async fn poll_channels_tier3_rotating_runs_batch_concurrently_and_unmarks_transient_failures() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use tokio::sync::Barrier;

        // 5 channels in batch 1 (out of 6 total) so the round does not wrap yet and we can
        // verify ch3 (transient 500) is unmarked while ch1/ch2/ch4/ch5 stay marked polled.
        let barrier = Arc::new(Barrier::new(5));
        let ok_handler = |id: &'static str, base: String, barrier: Arc<Barrier>| {
            move || {
                let barrier = Arc::clone(&barrier);
                let link = format!("{base}/teams/t1/channels/{id}/messages/delta?s=1");
                async move {
                    barrier.wait().await;
                    Json(serde_json::json!({
                        "value": [{
                            "id": format!("m-{id}"),
                            "createdDateTime": "2026-11-20T07:00:00Z",
                            "messageType": "message",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "text", "content": format!("msg {id}") }
                        }],
                        "@odata.deltaLink": link
                    }))
                }
            }
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let b3 = Arc::clone(&barrier);

        let app = Router::new()
            .route(
                "/v1.0/teams/t1/channels/ch1/messages/delta",
                get(ok_handler("ch1", base.clone(), Arc::clone(&barrier))),
            )
            .route(
                "/v1.0/teams/t1/channels/ch2/messages/delta",
                get(ok_handler("ch2", base.clone(), Arc::clone(&barrier))),
            )
            .route(
                "/v1.0/teams/t1/channels/ch3/messages/delta",
                get(move || {
                    let barrier = Arc::clone(&b3);
                    async move {
                        barrier.wait().await;
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({
                                "error": { "code": "InternalServerError", "message": "boom" }
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch4/messages/delta",
                get(ok_handler("ch4", base.clone(), Arc::clone(&barrier))),
            )
            .route(
                "/v1.0/teams/t1/channels/ch5/messages/delta",
                get(ok_handler("ch5", base.clone(), Arc::clone(&barrier))),
            );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let graph = regression_graph(addr);
        let storage = Storage::open_in_memory().unwrap();
        let mk_ch = |id: &str, name: &str| crate::models::ChannelSummary {
            id: id.to_string(),
            team_id: "t1".to_string(),
            team_name: "Eng".to_string(),
            display_name: name.to_string(),
            description: None,
            membership_type: "standard".to_string(),
            web_url: None,
        };
        storage
            .upsert_teams(&[TeamSummary {
                id: "t1".to_string(),
                display_name: "Eng".to_string(),
                description: None,
                channels: vec![
                    mk_ch("ch1", "01-Alpha"),
                    mk_ch("ch2", "02-Beta"),
                    mk_ch("ch3", "03-Gamma"),
                    mk_ch("ch4", "04-Delta"),
                    mk_ch("ch5", "05-Epsilon"),
                    mk_ch("ch6", "06-Zeta"),
                ],
            }])
            .unwrap();

        let (event_tx, _event_rx) = mpsc::unbounded_channel();
        let pinned = HashSet::new();
        let mut rotation = ChannelRotationState::default();
        let mut restricted = HashSet::new();

        let res = tokio::time::timeout(
            Duration::from_secs(3),
            poll_channels_tier3_rotating(
                &graph,
                &storage,
                &event_tx,
                None,
                &pinned,
                None,
                &mut rotation,
                &mut restricted,
            ),
        )
        .await
        .expect("channel rotation batch must execute concurrently");
        assert!(res.is_err(), "transient 500 must be surfaced to backoff tracker");
        assert!(storage.get_message("chan:t1:ch1", "m-ch1").unwrap().is_some());
        assert!(storage.get_message("chan:t1:ch2", "m-ch2").unwrap().is_some());
        assert!(storage.get_message("chan:t1:ch4", "m-ch4").unwrap().is_some());
        assert!(storage.get_message("chan:t1:ch5", "m-ch5").unwrap().is_some());
        assert!(rotation.polled_in_round.contains("chan:t1:ch1"));
        assert!(rotation.polled_in_round.contains("chan:t1:ch2"));
        assert!(rotation.polled_in_round.contains("chan:t1:ch4"));
        assert!(rotation.polled_in_round.contains("chan:t1:ch5"));
        assert!(
            !rotation.polled_in_round.contains("chan:t1:ch3"),
            "failed channel must be unmarked so it retries on the next tick"
        );
        assert!(restricted.is_empty());

        server.abort();
    }

    // Track 5: a fresh page-1 /me/chats response must be able to replace a stale EOF (saved
    // when the list was shorter) and a cleared, expired link; a later-page link is kept.
    #[test]
    fn chats_list_cursor_page_one_replaces_stale_eof_and_cleared_link() {
        let storage = Storage::open_in_memory().unwrap();
        let cursor = |storage: &Storage| {
            storage
                .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
                .unwrap()
                .expect("chat-list cursor saved")
        };
        let save = |delta_link: Option<&str>, oldest: Option<&str>| {
            storage
                .save_sync_cursor(&SyncCursor {
                    scope_key: SyncCursor::CHATS_LIST_SCOPE.to_string(),
                    delta_link: delta_link.map(str::to_string),
                    oldest_msg_ts: oldest.map(str::to_string),
                    synced_at: chrono::Utc::now().to_rfc3339(),
                })
                .unwrap();
        };
        let page2 = "https://graph.microsoft.com/v1.0/me/chats?$skiptoken=page2";

        // 1) Stored EOF + page 1 now has a nextLink: paging resumes.
        save(None, Some(SyncCursor::HISTORY_EOF));
        save_initial_chats_list_cursor(&storage, Some(page2.to_string())).unwrap();
        let resumed = cursor(&storage);
        assert_eq!(resumed.delta_link.as_deref(), Some(page2));
        assert!(!resumed.is_history_eof());

        // 2) A link saved by later paging is not rewound by another page-1 response.
        let page5 = "https://graph.microsoft.com/v1.0/me/chats?$skiptoken=page5";
        save(Some(page5), None);
        save_initial_chats_list_cursor(&storage, Some(page2.to_string())).unwrap();
        assert_eq!(cursor(&storage).delta_link.as_deref(), Some(page5));
        save_initial_chats_list_cursor(&storage, None).unwrap();
        assert_eq!(cursor(&storage).delta_link.as_deref(), Some(page5));

        // 3) An expired link cleared by load_older_chats is replaced by the fresh nextLink.
        save(None, None);
        save_initial_chats_list_cursor(&storage, Some(page2.to_string())).unwrap();
        assert_eq!(cursor(&storage).delta_link.as_deref(), Some(page2));

        // 4) A blank nextLink counts as none: page 1 is the whole list.
        save(Some("   "), None);
        save_initial_chats_list_cursor(&storage, Some("  ".to_string())).unwrap();
        let eof = cursor(&storage);
        assert!(eof.is_history_eof());
        assert_eq!(eof.delta_link, None);
    }
}
