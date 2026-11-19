//! TUI entry shell: terminal raw mode, mouse capture, keyboard enhancement, event loop, external editor.

use std::io::{self, Stdout};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CrosstermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement, BeginSynchronizedUpdate,
    EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{mpsc, watch};

use crate::auth::AuthManager;
use crate::config::{AppConfig, Persona};
use crate::graph::GraphClient;
use crate::models::CoreEvent;
use crate::storage::Storage;
use crate::sync::{self, FocusTarget, SyncCommand, SyncController};
use crate::ui::{
    self, action,
    action::Action,
    app::{AppState, UiMsg, UiState},
    composer, event,
};

struct TerminalGuard {
    keyboard_enhanced: bool,
    kitty_used: bool,
}

impl TerminalGuard {
    fn enter() -> Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let keyboard_enhanced = if supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            )
            .is_ok()
        } else {
            false
        };

        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.hide_cursor()?;
        terminal.clear()?;

        Ok((
            Self {
                keyboard_enhanced,
                kitty_used: false,
            },
            terminal,
        ))
    }

    fn restore(&self) {
        use std::io::Write;
        let mut stdout = io::stdout();
        if self.kitty_used {
            let _ = stdout.write_all(crate::img_preview::kitty_delete_preview_sequence().as_bytes());
            let _ = stdout.flush();
        }
        if self.keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn install_panic_hook(keyboard_enhanced: bool) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut stdout = io::stdout();
        if keyboard_enhanced {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        prev(info);
    }));
}

const MAX_COALESCED_EVENTS: usize = 64;
const ACTIVITY_THROTTLE: Duration = Duration::from_secs(5);
const PASTE_BURST_POLL: Duration = Duration::from_millis(2);
const MAX_PASTE_BURST_EVENTS: usize = 4096;

#[derive(Debug, Default)]
struct PasteBurstDetector {
    keys: Vec<KeyEvent>,
}

impl PasteBurstDetector {
    fn is_candidate_key(key: &KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match key.code {
            KeyCode::Enter => key.modifiers == KeyModifiers::NONE,
            KeyCode::Char(ch) => {
                !ch.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            }
            _ => false,
        }
    }

    fn push(&mut self, key: KeyEvent) {
        self.keys.push(key);
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    fn into_paste_or_keys(mut self) -> std::result::Result<String, Vec<KeyEvent>> {
        let has_enter = self
            .keys
            .iter()
            .any(|k| k.code == KeyCode::Enter && k.modifiers == KeyModifiers::NONE);
        let has_char = self
            .keys
            .iter()
            .any(|k| matches!(k.code, KeyCode::Char(_)));
        if self.keys.len() >= 2 && has_enter && has_char {
            let mut out = String::with_capacity(self.keys.len());
            for k in self.keys.drain(..) {
                match k.code {
                    KeyCode::Char(ch) => out.push(ch),
                    KeyCode::Enter => out.push('\n'),
                    _ => {}
                }
            }
            Ok(out)
        } else {
            Err(self.keys)
        }
    }
}

enum LoopStep {
    Continue,
    Quit,
}

async fn process_terminal_event_with_burst(
    first_ev: CrosstermEvent,
    events: &mut EventStream,
    app: &mut AppState,
    ui: &mut UiState,
    dirty: &mut bool,
    last_activity_sent: &mut Option<Instant>,
) -> LoopStep {
    let CrosstermEvent::Key(first_key) = first_ev else {
        return handle_terminal_event(first_ev, app, ui, dirty, last_activity_sent);
    };
    if !ui.accepts_text_input() || !PasteBurstDetector::is_candidate_key(&first_key) {
        return handle_terminal_event(
            CrosstermEvent::Key(first_key),
            app,
            ui,
            dirty,
            last_activity_sent,
        );
    }

    let mut detector = PasteBurstDetector::default();
    detector.push(first_key);
    let mut trailing_event: Option<CrosstermEvent> = None;
    let mut stream_ended = false;

    while detector.len() < MAX_PASTE_BURST_EVENTS {
        let has_enter = detector
            .keys
            .iter()
            .any(|k| k.code == KeyCode::Enter && k.modifiers == KeyModifiers::NONE);
        let next_ev = match events.next().now_or_never() {
            Some(item) => item,
            None => {
                let buffered = crossterm::event::poll(Duration::ZERO).unwrap_or(false);
                if !has_enter && !buffered {
                    break;
                }
                match tokio::time::timeout(PASTE_BURST_POLL, events.next()).await {
                    Ok(item) => item,
                    Err(_) => break,
                }
            }
        };
        match next_ev {
            Some(Ok(CrosstermEvent::Key(k))) if k.kind == KeyEventKind::Release => {
                continue;
            }
            Some(Ok(CrosstermEvent::Key(k))) if PasteBurstDetector::is_candidate_key(&k) => {
                detector.push(k);
            }
            Some(Ok(other)) => {
                trailing_event = Some(other);
                break;
            }
            Some(Err(_)) | None => {
                stream_ended = true;
                break;
            }
        }
    }

    match detector.into_paste_or_keys() {
        Ok(coalesced) => {
            notify_activity_throttled(app, last_activity_sent);
            *dirty = true;
            if action::dispatch(Action::Paste(coalesced), app, ui) {
                return LoopStep::Quit;
            }
        }
        Err(keys) => {
            for k in keys {
                if matches!(
                    handle_terminal_event(
                        CrosstermEvent::Key(k),
                        app,
                        ui,
                        dirty,
                        last_activity_sent
                    ),
                    LoopStep::Quit
                ) {
                    return LoopStep::Quit;
                }
            }
        }
    }

    if let Some(extra) = trailing_event {
        if matches!(
            handle_terminal_event(extra, app, ui, dirty, last_activity_sent),
            LoopStep::Quit
        ) {
            return LoopStep::Quit;
        }
    }

    if stream_ended {
        LoopStep::Quit
    } else {
        LoopStep::Continue
    }
}

fn notify_activity_throttled(app: &AppState, last_activity_sent: &mut Option<Instant>) {
    if last_activity_sent.is_none_or(|t| t.elapsed() >= ACTIVITY_THROTTLE) {
        app.sync_ctrl.request(SyncCommand::NotifyUserActivity);
        *last_activity_sent = Some(Instant::now());
    }
}

fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut AppState,
    ui: &mut UiState,
    guard: &mut TerminalGuard,
) -> Result<()> {
    use std::io::Write;
    use crate::img_preview::{self, TerminalImageProtocol};

    let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
    let prev_graphic = ui.active_pixel_graphic.clone();

    let draw_res: Result<()> = (|| {
        let mut cursor_pos = None;
        terminal.draw(|frame| {
            cursor_pos = ui::draw(frame, app, ui);
        })?;

        let pending_matches_prev = match (&prev_graphic, &ui.pending_pixel_graphic) {
            (None, None) => true,
            (Some((pk, pr, pp)), Some(pg)) => {
                pk == &pg.cache_key && pr == &pg.rect && pp == &pg.protocol
            }
            _ => false,
        };

        if !pending_matches_prev {
            if let Some((_, _, prev_proto)) = &prev_graphic {
                if *prev_proto == TerminalImageProtocol::Kitty {
                    terminal
                        .backend_mut()
                        .write_all(img_preview::kitty_delete_preview_sequence().as_bytes())?;
                }
                terminal.clear()?;
                ui.active_pixel_graphic = None;
                terminal.draw(|frame| {
                    cursor_pos = ui::draw(frame, app, ui);
                })?;
            }

            if let Some(pg) = ui.pending_pixel_graphic.clone() {
                let already_active = ui.active_pixel_graphic.as_ref().is_some_and(
                    |(ak, ar, ap)| ak == &pg.cache_key && *ar == pg.rect && *ap == pg.protocol,
                );
                if !already_active {
                    if let Some(payload) = app
                        .image_cache
                        .get(&pg.cache_key)
                        .and_then(|r| r.pixel_payload.as_deref())
                    {
                        if pg.protocol == TerminalImageProtocol::Kitty {
                            terminal
                                .backend_mut()
                                .write_all(img_preview::kitty_delete_preview_sequence().as_bytes())?;
                            guard.kitty_used = true;
                        }
                        crossterm::queue!(
                            terminal.backend_mut(),
                            crossterm::cursor::MoveTo(pg.rect.x, pg.rect.y)
                        )?;
                        terminal.backend_mut().write_all(payload.as_bytes())?;
                        terminal.backend_mut().flush()?;
                        ui.active_pixel_graphic = Some((pg.cache_key, pg.rect, pg.protocol));
                    } else {
                        ui.active_pixel_graphic = None;
                    }
                }
            } else {
                ui.active_pixel_graphic = None;
            }
        }

        if let Some((cx, cy)) = cursor_pos {
            let _ = terminal.show_cursor();
            let _ = terminal.set_cursor_position((cx, cy));
        } else {
            let _ = terminal.hide_cursor();
        }
        Ok(())
    })();

    let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
    draw_res
}

fn handle_terminal_event(
    ev: CrosstermEvent,
    app: &mut AppState,
    ui: &mut UiState,
    dirty: &mut bool,
    last_activity_sent: &mut Option<Instant>,
) -> LoopStep {
    match ev {
        CrosstermEvent::Key(key) => {
            let act = event::key_to_action_for_tab(
                key,
                ui.focus(),
                ui.active_nav_tab,
                &ui.keymap,
                ui.accepts_text_input(),
            )
            .or_else(|| {
                (key.kind == crossterm::event::KeyEventKind::Press
                    && key.code == KeyCode::BackTab)
                    .then_some(Action::PrevFocus)
            });
            if let Some(act) = act {
                notify_activity_throttled(app, last_activity_sent);
                *dirty = true;
                if action::dispatch(act, app, ui) {
                    return LoopStep::Quit;
                }
            }
        }
        CrosstermEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                notify_activity_throttled(app, last_activity_sent);
                *dirty = true;
                if action::dispatch(
                    Action::MouseScroll {
                        delta: -3,
                        column: mouse.column,
                        row: mouse.row,
                    },
                    app,
                    ui,
                ) {
                    return LoopStep::Quit;
                }
            }
            MouseEventKind::ScrollDown => {
                notify_activity_throttled(app, last_activity_sent);
                *dirty = true;
                if action::dispatch(
                    Action::MouseScroll {
                        delta: 3,
                        column: mouse.column,
                        row: mouse.row,
                    },
                    app,
                    ui,
                ) {
                    return LoopStep::Quit;
                }
            }
            _ => {
                if let Some(act) = event::mouse_to_action(mouse, &ui.hitmap) {
                    notify_activity_throttled(app, last_activity_sent);
                    *dirty = true;
                    if action::dispatch(act, app, ui) {
                        return LoopStep::Quit;
                    }
                }
            }
        },
        CrosstermEvent::Paste(text) => {
            notify_activity_throttled(app, last_activity_sent);
            *dirty = true;
            if action::dispatch(Action::Paste(text), app, ui) {
                return LoopStep::Quit;
            }
        }
        CrosstermEvent::Resize(w, h) => {
            ui.term_width = w;
            ui.term_height = h;
            *dirty = true;
            if matches!(
                ui.overlays.last(),
                Some(ui::overlays::Overlay::ImagePreview(_))
            ) {
                let _ = action::dispatch(Action::LoadPreviewImage, app, ui);
            }
        }
        _ => {}
    }
    LoopStep::Continue
}

fn spawn_sync_supervisor(
    config: AppConfig,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
    core_tx: mpsc::UnboundedSender<CoreEvent>,
    ui_tx: mpsc::UnboundedSender<UiMsg>,
    mut proxy_focus_rx: watch::Receiver<FocusTarget>,
    mut proxy_nudge_rx: mpsc::UnboundedReceiver<SyncCommand>,
) {
    tokio::spawn(async move {
        let expected_user_id = auth
            .current_tokens()
            .await
            .and_then(|t| t.user_id)
            .filter(|id| !id.is_empty());

        if core_tx.is_closed() {
            return;
        }

        if config.auto_daemon {
            let _ = crate::daemon::ensure_daemon_started(&config).await;
        }

        if core_tx.is_closed() {
            return;
        }

        let mut fell_back_from_daemon = false;

        if let Some(mut conn) =
            crate::daemon::try_connect_tui(&config, expected_user_id.as_deref()).await
        {
            let _ = ui_tx.send(UiMsg::DaemonAttached {
                pid: conn.status.pid,
                sync_busy: conn.status.sync_busy,
            });
            let initial_focus = proxy_focus_rx.borrow_and_update().clone();
            if initial_focus.target.is_some() || initial_focus.thread_root_id.is_some() {
                conn.sync_ctrl
                    .set_focus(initial_focus.target, initial_focus.thread_root_id);
            }

            loop {
                tokio::select! {
                    ev = conn.events_rx.recv() => {
                        match ev {
                            Some(core_ev) => {
                                if core_tx.send(core_ev).is_err() {
                                    return;
                                }
                            }
                            None => {
                                fell_back_from_daemon = true;
                                break;
                            }
                        }
                    }
                    cmd = proxy_nudge_rx.recv() => {
                        match cmd {
                            None | Some(SyncCommand::Shutdown) => return,
                            Some(cmd) => conn.sync_ctrl.request(cmd),
                        }
                    }
                    changed = proxy_focus_rx.changed() => {
                        if changed.is_err() {
                            return;
                        }
                        let focus = proxy_focus_rx.borrow_and_update().clone();
                        conn.sync_ctrl.set_focus(focus.target, focus.thread_root_id);
                    }
                }
            }
        }

        if core_tx.is_closed() {
            return;
        }

        let local_ctrl = sync::spawn_sync_engine(config, graph, storage, core_tx);
        if fell_back_from_daemon {
            let _ = ui_tx.send(UiMsg::DaemonDetached);
        } else {
            let _ = ui_tx.send(UiMsg::StartLocalBootstrap);
        }

        let current_focus = proxy_focus_rx.borrow_and_update().clone();
        if current_focus.target.is_some() || current_focus.thread_root_id.is_some() {
            local_ctrl.set_focus(current_focus.target, current_focus.thread_root_id);
        }

        loop {
            tokio::select! {
                cmd = proxy_nudge_rx.recv() => {
                    match cmd {
                        None | Some(SyncCommand::Shutdown) => {
                            local_ctrl.request(SyncCommand::Shutdown);
                            break;
                        }
                        Some(cmd) => local_ctrl.request(cmd),
                    }
                }
                changed = proxy_focus_rx.changed() => {
                    if changed.is_err() {
                        local_ctrl.request(SyncCommand::Shutdown);
                        break;
                    }
                    let focus = proxy_focus_rx.borrow_and_update().clone();
                    local_ctrl.set_focus(focus.target, focus.thread_root_id);
                }
            }
        }
    });
}

pub async fn run(
    config: AppConfig,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
) -> Result<()> {
    run_with_persona(config, Persona::detect(), auth, graph, storage).await
}

pub async fn run_with_persona(
    config: AppConfig,
    _persona: Persona,
    auth: AuthManager,
    graph: GraphClient,
    storage: Storage,
) -> Result<()> {
    // Enter alternate screen and raw mode first so the terminal responds in <5ms.
    let (mut guard, mut terminal) = TerminalGuard::enter()?;
    install_panic_hook(guard.keyboard_enhanced);

    let (core_tx, mut core_rx) = mpsc::unbounded_channel::<CoreEvent>();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();

    let (proxy_focus_tx, proxy_focus_rx) = watch::channel(FocusTarget::default());
    let (proxy_nudge_tx, proxy_nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(proxy_focus_tx, proxy_nudge_tx, false);

    let mut ui = UiState::from_config(&config);
    ui.graphics_caps = crate::img_preview::probe_terminal_graphics_caps(config.image_protocol);
    ui.kitty_keyboard_supported = guard.keyboard_enhanced;
    let mut app = AppState::new(
        config.clone(),
        auth.clone(),
        graph.clone(),
        storage.clone(),
        sync_ctrl.clone(),
        ui_tx.clone(),
    );
    app.load_cached_teams();
    draw_frame(&mut terminal, &mut app, &mut ui, &mut guard)?;

    spawn_sync_supervisor(
        config,
        auth,
        graph,
        storage,
        core_tx,
        ui_tx,
        proxy_focus_rx,
        proxy_nudge_rx,
    );

    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut last_activity_sent: Option<Instant> = None;

    loop {
        if ui.editor_requested {
            ui.editor_requested = false;
            if ui
                .active_pixel_graphic
                .as_ref()
                .is_some_and(|(_, _, p)| *p == crate::img_preview::TerminalImageProtocol::Kitty)
            {
                use std::io::Write;
                let _ = terminal
                    .backend_mut()
                    .write_all(crate::img_preview::kitty_delete_preview_sequence().as_bytes());
                let _ = terminal.backend_mut().flush();
            }
            ui.active_pixel_graphic = None;
            guard.restore();
            let result = composer::external_edit(ui.composer.text());
            let mut stdout = io::stdout();
            enable_raw_mode()?;
            execute!(
                stdout,
                EnterAlternateScreen,
                EnableMouseCapture,
                EnableBracketedPaste
            )?;
            if guard.keyboard_enhanced {
                let _ = execute!(
                    stdout,
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                            | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                    )
                );
            }
            terminal.clear()?;
            dirty = true;

            match result {
                Ok(edited) => {
                    ui.composer.load_text(&edited);
                    ui.info("Updated draft from external editor");
                }
                Err(err) => {
                    ui.error(format!("External editor error: {err}"));
                }
            }
        }

        if dirty {
            draw_frame(&mut terminal, &mut app, &mut ui, &mut guard)?;
            dirty = false;
        }

        let mut should_quit = false;
        tokio::select! {
            _ = tick.tick() => {
                let before = ui.toasts.len();
                ui.toasts.tick(std::time::Instant::now());
                if ui.toasts.len() != before {
                    dirty = true;
                }
                if ui.poll_retrying || app.syncing || ui.tasks.first_running().is_some() {
                    ui.spinner_tick = ui.spinner_tick.wrapping_add(1);
                    dirty = true;
                }
            }
            maybe_core = core_rx.recv() => {
                match maybe_core {
                    Some(ev) => {
                        app.handle_core_event(&mut ui, ev);
                        dirty = true;
                    }
                    None => should_quit = true,
                }
            }
            maybe_ui = ui_rx.recv() => {
                if let Some(msg) = maybe_ui {
                    app.handle_ui_msg(&mut ui, msg);
                    dirty = true;
                }
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(ev)) => {
                        if matches!(
                            process_terminal_event_with_burst(
                                ev,
                                &mut events,
                                &mut app,
                                &mut ui,
                                &mut dirty,
                                &mut last_activity_sent
                            )
                            .await,
                            LoopStep::Quit
                        ) {
                            should_quit = true;
                        }
                    }
                    Some(Err(_)) | None => should_quit = true,
                }
            }
        }
        if should_quit {
            break;
        }

        // Coalesce burst events (scroll wheel, fast typing, sync batches) before redrawing.
        let mut drained = 0usize;
        while drained < MAX_COALESCED_EVENTS && !ui.editor_requested {
            let mut progressed = false;
            while drained < MAX_COALESCED_EVENTS {
                match core_rx.try_recv() {
                    Ok(ev) => {
                        app.handle_core_event(&mut ui, ev);
                        dirty = true;
                        drained += 1;
                        progressed = true;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        should_quit = true;
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                }
            }
            if should_quit {
                break;
            }
            while drained < MAX_COALESCED_EVENTS {
                match ui_rx.try_recv() {
                    Ok(msg) => {
                        app.handle_ui_msg(&mut ui, msg);
                        dirty = true;
                        drained += 1;
                        progressed = true;
                    }
                    Err(_) => break,
                }
            }
            while drained < MAX_COALESCED_EVENTS && !ui.editor_requested {
                match events.next().now_or_never() {
                    Some(Some(Ok(ev))) => {
                        drained += 1;
                        progressed = true;
                        if matches!(
                            process_terminal_event_with_burst(
                                ev,
                                &mut events,
                                &mut app,
                                &mut ui,
                                &mut dirty,
                                &mut last_activity_sent
                            )
                            .await,
                            LoopStep::Quit
                        ) {
                            should_quit = true;
                            break;
                        }
                    }
                    Some(Some(Err(_)) | None) => {
                        should_quit = true;
                        break;
                    }
                    None => break,
                }
            }
            if should_quit || !progressed {
                break;
            }
        }
        if should_quit {
            break;
        }
    }

    app.stash_draft(&ui);
    let _ = sync_ctrl.request(SyncCommand::Shutdown);
    drop(guard);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn paste_burst_detector_coalesces_multiline_burst() {
        let mut d = PasteBurstDetector::default();
        for ch in ['h', 'i'] {
            let k = press(KeyCode::Char(ch), KeyModifiers::NONE);
            assert!(PasteBurstDetector::is_candidate_key(&k));
            d.push(k);
        }
        d.push(press(KeyCode::Enter, KeyModifiers::NONE));
        d.push(press(KeyCode::Char('b'), KeyModifiers::SHIFT));
        assert_eq!(d.into_paste_or_keys(), Ok("hi\nb".to_string()));
    }

    #[test]
    fn paste_burst_detector_preserves_single_enter_and_plain_typing() {
        let mut single_enter = PasteBurstDetector::default();
        single_enter.push(press(KeyCode::Enter, KeyModifiers::NONE));
        assert!(single_enter.into_paste_or_keys().is_err());

        let mut plain_typing = PasteBurstDetector::default();
        plain_typing.push(press(KeyCode::Char('a'), KeyModifiers::NONE));
        plain_typing.push(press(KeyCode::Char('b'), KeyModifiers::NONE));
        assert!(plain_typing.into_paste_or_keys().is_err());

        let mut only_enters = PasteBurstDetector::default();
        only_enters.push(press(KeyCode::Enter, KeyModifiers::NONE));
        only_enters.push(press(KeyCode::Enter, KeyModifiers::NONE));
        assert!(only_enters.into_paste_or_keys().is_err());

        assert!(!PasteBurstDetector::is_candidate_key(&press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(!PasteBurstDetector::is_candidate_key(&press(
            KeyCode::Enter,
            KeyModifiers::ALT
        )));
    }
}
