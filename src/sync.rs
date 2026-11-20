//! Adaptive 2-lane background synchronization engine for Microsoft Teams (Fast Interactive Lane + Background Poll Lane).

use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
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
        NotificationCandidate, PresenceStatus, SyncCursor, POLL_RETRY_SUMMARY,
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
    let lower = format!("{err:#}").to_ascii_lowercase();
    let detail = extract_error_detail_for_status(&lower);

    if detail.contains("403")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient")
        || lower.contains("aclcheckfailed")
    {
        return SyncErrorKind::Forbidden403;
    }

    if detail.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("token expired")
    {
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

#[derive(Debug, Default)]
pub(crate) struct PeriodicBackoffTracker {
    tick_count: u64,
    last_activity_seq: u64,
}

impl PeriodicBackoffTracker {
    pub(crate) fn on_tick(&mut self, idle_duration: Duration, activity_seq: u64) -> bool {
        if activity_seq != self.last_activity_seq {
            self.last_activity_seq = activity_seq;
            self.tick_count = 0;
        }
        let mult = idle_multiplier(idle_duration);
        if mult <= 1 {
            self.tick_count = 0;
            return true;
        }
        self.tick_count = self.tick_count.saturating_add(1);
        self.tick_count % mult == 0
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
        self.shared.record_activity(Instant::now());
        self.request(SyncCommand::NotifyUserActivity);
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
            hex::encode(hasher.finalize())
        }
        None => String::new(),
    }
}

/// Performs an initial bootstrap sync (chats + joined teams/channels) into SQLite.
pub async fn bootstrap_conversations(graph: &GraphClient, storage: &Storage) -> Result<()> {
    let (chats_res, teams_res) = tokio::join!(
        graph.list_chats_page(50, None),
        graph.list_joined_teams_with_channels(),
    );
    let (chats, next_link) = chats_res?;
    run_storage(storage, move |st| {
        st.upsert_conversations(&chats)?;
        save_initial_chats_list_cursor_if_absent(st, next_link)
    })
    .await?;
    if let Ok(teams) = teams_res {
        let _ = run_storage(storage, move |st| st.upsert_teams(&teams)).await;
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
        SyncErrorKind::Forbidden403 => {
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

        // Consume initial immediate tick so startup isn't flooded simultaneously before focus/commands arrive.
        active_tick.tick().await;

        loop {
            if *fast_shutdown_rx.borrow() {
                break;
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
                            match sync_active_focus_with_state(
                                &focus,
                                &fast_graph,
                                &fast_storage,
                                &fast_event_tx,
                                &mut last_active_sig,
                                false,
                            ).await {
                                Ok(()) => {
                                    if active_err_backoff.record_success() {
                                        let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                            busy: false,
                                            summary: "Synced".to_string(),
                                        });
                                    }
                                }
                                Err(err) => {
                                    handle_lane1_error(
                                        &err,
                                        &focus,
                                        &fast_shared,
                                        &mut restricted_convs,
                                        &mut active_err_backoff,
                                        &fast_event_tx,
                                    );
                                }
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
                            fast_shared.record_activity(Instant::now());
                        }
                    }
                }
                changed = focus_rx.changed() => {
                    if changed.is_err() {
                        let _ = shutdown_tx.send(true);
                        break;
                    }
                    let now = Instant::now();
                    fast_shared.record_activity(now);
                    let focus = focus_rx.borrow_and_update().clone();
                    let is_restricted = focus
                        .target
                        .as_ref()
                        .is_some_and(|t| restricted_convs.contains(&t.conv_key()));
                    if !is_restricted && !fast_shared.is_auth_paused(now) {
                        match sync_active_focus_with_state(
                            &focus,
                            &fast_graph,
                            &fast_storage,
                            &fast_event_tx,
                            &mut last_active_sig,
                            false,
                        ).await {
                            Ok(()) => {
                                if active_err_backoff.record_success() {
                                    let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                        busy: false,
                                        summary: "Synced".to_string(),
                                    });
                                }
                            }
                            Err(err) => {
                                handle_lane1_error(
                                    &err,
                                    &focus,
                                    &fast_shared,
                                    &mut restricted_convs,
                                    &mut active_err_backoff,
                                    &fast_event_tx,
                                );
                            }
                        }
                    }
                    active_tick.reset();
                }
                _ = active_tick.tick() => {
                    let now = Instant::now();
                    let (idle, seq) = fast_shared.idle_snapshot(now);
                    if active_backoff.on_tick(idle, seq)
                        && !fast_shared.is_auth_paused(now)
                        && !active_err_backoff.is_backed_off(now)
                    {
                        let focus = focus_rx.borrow().clone();
                        let allowed = focus
                            .target
                            .as_ref()
                            .is_some_and(|t| !restricted_convs.contains(&t.conv_key()));
                        if allowed {
                            match sync_active_focus_with_state(
                                &focus,
                                &fast_graph,
                                &fast_storage,
                                &fast_event_tx,
                                &mut last_active_sig,
                                true,
                            ).await {
                                Ok(()) => {
                                    if active_err_backoff.record_success() {
                                        let _ = fast_event_tx.send(CoreEvent::SyncStateChanged {
                                            busy: false,
                                            summary: "Synced".to_string(),
                                        });
                                    }
                                }
                                Err(err) => {
                                    handle_lane1_error(
                                        &err,
                                        &focus,
                                        &fast_shared,
                                        &mut restricted_convs,
                                        &mut active_err_backoff,
                                        &fast_event_tx,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    });

    // Lane 2: Background Poll Lane (chat preview diff + $batch, presence polling, bulk refreshes)
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
        let mut presence_cache: PresencePollCache = HashMap::new();
        let mut presence_disabled_for_token: Option<String> = None;
        let mut chat_err_backoff = ErrorBackoffState::new(chat_ms);
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
                            chat_err_backoff.reset_window();
                            presence_err_backoff.reset_window();
                            let active_key = bg_focus_rx
                                .borrow()
                                .target
                                .as_ref()
                                .map(ConversationTarget::conv_key);
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
                                active_key.as_deref(),
                            ).await {
                                Ok(()) => {
                                    chat_err_backoff.record_success();
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
                                let was_degraded = chat_err_backoff.is_degraded() || presence_err_backoff.is_degraded();
                                chat_err_backoff.record_success();
                                let now_degraded = chat_err_backoff.is_degraded() || presence_err_backoff.is_degraded();
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
                                    SyncErrorKind::Transient => {
                                        let was_degraded = chat_err_backoff.is_degraded() || presence_err_backoff.is_degraded();
                                        chat_err_backoff.record_transient_failure(now);
                                        let now_degraded = chat_err_backoff.is_degraded() || presence_err_backoff.is_degraded();
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
                    if idle < Duration::from_secs(IDLE_THRESHOLD_SECS)
                        && channel_backoff.on_tick(idle, seq)
                        && !bg_shared.is_auth_paused(now)
                        && !presence_err_backoff.is_backed_off(now)
                    {
                        let token_fp = current_token_fingerprint(&graph).await;
                        if presence_disabled_for_token.as_ref() == Some(&token_fp) {
                            continue;
                        }
                        presence_disabled_for_token = None;
                        match poll_presences_tier3_with_cache(
                            &graph,
                            &storage,
                            &event_tx,
                            &mut presence_cache,
                        ).await {
                            Ok(()) => {
                                let was_degraded = chat_err_backoff.is_degraded() || presence_err_backoff.is_degraded();
                                presence_err_backoff.record_success();
                                let now_degraded = chat_err_backoff.is_degraded() || presence_err_backoff.is_degraded();
                                if was_degraded && !now_degraded {
                                    let _ = event_tx.send(CoreEvent::SyncStateChanged {
                                        busy: false,
                                        summary: "Synced".to_string(),
                                    });
                                }
                            }
                            Err(err) => {
                                match classify_sync_error(&err) {
                                    SyncErrorKind::Forbidden403 => {
                                        presence_disabled_for_token = Some(token_fp);
                                    }
                                    SyncErrorKind::Unauthorized401 => {
                                        bg_shared.pause_auth(now, Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
                                    }
                                    SyncErrorKind::Transient => {
                                        let was_degraded = chat_err_backoff.is_degraded() || presence_err_backoff.is_degraded();
                                        presence_err_backoff.record_transient_failure(now);
                                        let now_degraded = chat_err_backoff.is_degraded() || presence_err_backoff.is_degraded();
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

fn save_initial_history_cursor_if_absent(
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

fn save_initial_chats_list_cursor_if_absent(
    storage: &Storage,
    next_link: Option<String>,
) -> Result<()> {
    if storage
        .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)?
        .is_none()
    {
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
        })?;
    }
    Ok(())
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

async fn sync_active_focus_with_state(
    focus: &FocusTarget,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    last_active_sig: &mut HashMap<String, u64>,
    is_periodic: bool,
) -> Result<()> {
    let Some(target) = &focus.target else {
        return Ok(());
    };
    let conv_key = target.conv_key();

    match target {
        ConversationTarget::Chat { .. } => {
            let (messages, next_link) = graph.list_messages_page(target, 50, None).await?;
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
                            .list_thread_replies(team_id, channel_id, root_id, 50)
                            .await,
                    )),
                    None => None,
                }
            };
            let (delta_res, replies_opt) = tokio::join!(delta_fut, replies_fut);

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
                        synced_at: chrono::Utc::now().to_rfc3339(),
                    };

                    let sig = compute_messages_signature(&messages);
                    let prev_sig = last_active_sig.get(&conv_key).copied();

                    let msgs_for_db = messages.clone();
                    let outcome = run_storage(storage, move |st| {
                        // Commit message upserts BEFORE advancing the delta cursor so delta updates
                        // can never be skipped or permanently lost.
                        let outcome = if !msgs_for_db.is_empty() {
                            st.upsert_messages_detailed(&msgs_for_db)?
                        } else {
                            Default::default()
                        };
                        st.save_sync_cursor(&new_cursor)?;
                        Ok(outcome)
                    })
                    .await?;

                    if !messages.is_empty() {
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
                    // Clear stale delta token before falling back to page listing.
                    let stale_key = conv_key.clone();
                    let stale_oldest = cursor.as_ref().and_then(|c| c.oldest_msg_ts.clone());
                    run_storage(storage, move |st| {
                        st.save_sync_cursor(&SyncCursor {
                            scope_key: stale_key,
                            delta_link: None,
                            oldest_msg_ts: stale_oldest,
                            synced_at: chrono::Utc::now().to_rfc3339(),
                        })
                    })
                    .await?;

                    let (messages, next_link) = graph.list_messages_page(target, 50, None).await?;
                    let sig = compute_messages_signature(&messages);
                    let prev_sig = last_active_sig.get(&conv_key).copied();
                    if !(is_periodic && prev_sig == Some(sig)) {
                        let msgs_for_db = messages.clone();
                        let key_for_db = conv_key.clone();
                        let changed_rows = run_storage(storage, move |st| {
                            let changed = st.upsert_messages(&msgs_for_db)?;
                            save_initial_history_cursor_if_absent(
                                st,
                                &key_for_db,
                                &msgs_for_db,
                                next_link,
                            )?;
                            Ok(changed)
                        })
                        .await?;
                        last_active_sig.insert(conv_key.clone(), sig);

                        if !is_periodic || prev_sig.is_none() || changed_rows > 0 {
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
                let replies = replies_res?;
                let thread_sig_key = format!("{conv_key}|thread:{root_id}");
                let sig = compute_messages_signature(&replies);
                let prev_sig = last_active_sig.get(&thread_sig_key).copied();
                if !(is_periodic && prev_sig == Some(sig)) {
                    let replies_for_db = replies.clone();
                    let changed_rows =
                        run_storage(storage, move |st| st.upsert_messages(&replies_for_db))
                            .await?;
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

    let (chats, next_link) = graph.list_chats_page(50, None).await?;

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
                st.upsert_conversations(&to_upsert)?;
            }
            if save_cursor {
                save_initial_chats_list_cursor_if_absent(st, next_link)?;
            }
            Ok(())
        })
        .await?;
        *chats_cursor_initialized = true;
    }

    let mut any_preview_updated = false;
    if !fetch_ids.is_empty() {
        let fetched = graph.batch_fetch_chat_messages(&fetch_ids, 20).await?;
        for (chat_id, messages) in fetched {
            let conv_key = ConversationTarget::Chat {
                chat_id: chat_id.clone(),
            }
            .conv_key();
            let conv_title = chat_by_id
                .get(&chat_id)
                .map(|c| c.title.clone())
                .or_else(|| chat_cache.get(&conv_key).map(|e| e.title.clone()))
                .unwrap_or_else(|| conv_key.clone());

            let (outcome, changed_messages, newly_inserted_messages) =
                run_storage(storage, move |st| {
                    let outcome = st.upsert_messages_detailed(&messages)?;
                    let inserted = filter_changed_messages_ref(&messages, &outcome.inserted_ids);
                    let changed = filter_changed_messages(messages, &outcome.changed_ids);
                    Ok((outcome, changed, inserted))
                })
                .await?;
            if !outcome.preview_updated_convs.is_empty() {
                any_preview_updated = true;
            }
            if !outcome.changed_ids.is_empty() {
                let notify_candidate =
                    build_notification_candidate(&conv_key, &conv_title, &newly_inserted_messages);
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

    for chat in &changed_chats {
        // Keep deferred dirty chats marked stale in `chat_cache` so the next tick fetches them.
        if deferred_dirty_keys.contains(&chat.conv_key) {
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

async fn refresh_all_conversations_with_cache(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    chat_cache: &mut ChatPollCache,
    chat_cache_seeded: &mut bool,
    chats_cursor_initialized: &mut bool,
    rolling_cursor: &mut usize,
    active_conv_key: Option<&str>,
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
    chats_res?;
    if let Ok(teams) = teams_res {
        let teams_for_db = teams.clone();
        run_storage(storage, move |st| st.upsert_teams(&teams_for_db)).await?;

        // Explicit refresh also reconciles inactive joined channels via delta so new posts,
        // @me mentions, deletions, and unread counts surface without focusing each channel.
        let mut refreshed_channels = 0usize;
        for team in &teams {
            for ch in &team.channels {
                if refreshed_channels >= MAX_REFRESH_CHANNELS {
                    break;
                }
                refreshed_channels += 1;
                let conv_key = ch.conv_key();
                let is_active = active_conv_key == Some(conv_key.as_str());
                let conv_title = format!("{} › #{}", team.display_name, ch.display_name);
                let _ = refresh_channel_delta_once(
                    graph,
                    storage,
                    event_tx,
                    &ch.team_id,
                    &ch.id,
                    &conv_key,
                    &conv_title,
                    !is_active,
                )
                .await;
            }
        }

        let all_convs = run_storage(storage, |st| st.list_conversations()).await?;
        let _ = event_tx.send(CoreEvent::TeamsUpdated(teams));
        let _ = event_tx.send(CoreEvent::ConversationsUpdated(all_convs));
    }
    Ok(())
}

async fn refresh_channel_delta_once(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    team_id: &str,
    channel_id: &str,
    conv_key: &str,
    conv_title: &str,
    mark_unread_on_new: bool,
) -> Result<()> {
    let ck = conv_key.to_string();
    let (cursor, had_local_msgs) = run_storage(storage, move |st| {
        let cur = st.get_sync_cursor(&ck)?;
        let has_msgs = !st.list_root_messages(&ck, 1, None)?.is_empty();
        Ok((cur, has_msgs))
    })
    .await?;
    let had_prior_state = cursor.is_some() || had_local_msgs;

    let delta_res = graph
        .sync_channel_messages_delta(
            team_id,
            channel_id,
            cursor.as_ref().and_then(|c| c.delta_link.as_deref()),
        )
        .await;

    let (messages, new_delta_link) = match delta_res {
        Ok(res) => res,
        Err(err) if is_expired_delta_error(&err) => {
            let target = ConversationTarget::Channel {
                team_id: team_id.to_string(),
                channel_id: channel_id.to_string(),
            };
            let (msgs, _) = graph.list_messages_page(&target, 50, None).await?;
            (msgs, None)
        }
        Err(err) => return Err(err),
    };

    let oldest_ts = messages
        .iter()
        .map(|m| m.created_at.as_str())
        .min()
        .map(str::to_string)
        .or_else(|| cursor.as_ref().and_then(|c| c.oldest_msg_ts.clone()));
    let effective_delta_link =
        new_delta_link.or_else(|| cursor.as_ref().and_then(|c| c.delta_link.clone()));
    let new_cursor = SyncCursor {
        scope_key: conv_key.to_string(),
        delta_link: effective_delta_link,
        oldest_msg_ts: oldest_ts,
        synced_at: chrono::Utc::now().to_rfc3339(),
    };

    let ck_db = conv_key.to_string();
    let msgs_for_db = messages.clone();
    let (outcome, newly_inserted) = run_storage(storage, move |st| {
        let outcome = if !msgs_for_db.is_empty() {
            st.upsert_messages_detailed(&msgs_for_db)?
        } else {
            Default::default()
        };
        st.save_sync_cursor(&new_cursor)?;
        let inserted = filter_changed_messages_ref(&msgs_for_db, &outcome.inserted_ids);
        if mark_unread_on_new && had_prior_state {
            let unread_delta = inserted
                .iter()
                .filter(|m| !m.is_me && !m.deleted)
                .count() as i32;
            if unread_delta > 0 {
                st.increment_conversation_unread(&ck_db, unread_delta)?;
            }
        }
        Ok((outcome, inserted))
    })
    .await?;

    if !outcome.changed_ids.is_empty() {
        let notify_candidate = if mark_unread_on_new && had_prior_state {
            build_notification_candidate(conv_key, conv_title, &newly_inserted)
        } else {
            None
        };
        let _ = event_tx.send(CoreEvent::MessagesUpdated {
            conv_key: conv_key.to_string(),
            parent_msg_id: None,
            messages,
            notify_candidate,
            changed_msg_ids: Some(outcome.changed_ids),
        });
    }
    Ok(())
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
        save_initial_chats_list_cursor_if_absent(&storage, Some(chats_next.to_string())).unwrap();
        let chats_cursor = storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .unwrap()
            .unwrap();
        assert_eq!(chats_cursor.delta_link.as_deref(), Some(chats_next));
        assert!(!chats_cursor.is_history_eof());

        save_initial_chats_list_cursor_if_absent(&storage, None).unwrap();
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
        let st_chats = Arc::clone(&stage);
        let st_batch = Arc::clone(&stage);
        let st_active = Arc::clone(&stage);
        let st_delta = Arc::clone(&stage);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let delta_base = format!("http://{addr}/v1.0/teams/t1/channels/ch1/messages/delta");

        let app = Router::new()
            .route(
                "/v1.0/me/chats",
                get(move || {
                    let st = Arc::clone(&st_chats);
                    async move {
                        let s = st.load(Ordering::SeqCst);
                        // Keep lastMessagePreview.id AND lastUpdatedDateTime identical across stages
                        // to prove rolling batch still catches edits/deletes on inactive chats.
                        let preview_body = if s == 0 {
                            "<p>bg-last-original</p>"
                        } else {
                            "<p>bg-last-original</p>"
                        };
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
                    }
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
                            if url.contains("/teams/t1/channels") {
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
            Some("chat:c-rx"),
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
}
