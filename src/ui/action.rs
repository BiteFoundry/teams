//! Central `Action` enum and single `dispatch(action, app, ui) -> bool` reducer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use ratatui::layout::Position;

use crate::models::ConversationTarget;
use crate::sync::SyncCommand;

use super::app::{AppState, Focus, NarrowPane, UiState};
use super::composer::{self, ReplyPreview};
use super::hitmap::{ButtonId, HitTarget, MessageScope};
use super::nav_pane::{self, NavRow, TAB_CHATS, TAB_COUNT};
use super::overlays::context_menu::{ContextMenu, ContextTarget};
use super::overlays::html_preview::{adaptive_card_to_html, HtmlPreviewOverlay};
use super::overlays::image_preview::ImagePreviewOverlay;
use super::overlays::links::{self, LinksOverlay};
use super::overlays::search::SearchOverlay;
use super::overlays::{Overlay, OverlayKind, OverlayOutcome};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    QuitConfirm,
    Noop,

    // Focus & layout
    Focus(Focus),
    NextFocus,
    PrevFocus,
    ToggleNavRail,
    ToggleAutoThreadPane,
    ToggleNarrowPane,
    SelectNarrowPane(NarrowPane),
    ToggleNotify,
    ToggleDensity,
    CycleTheme,
    ShowDownloadDir,

    // Navigation pane
    SelectNavTab(usize),
    SelectNavRow(usize),
    NavFirst,
    NavLast,
    NavCollapseOrParent,
    NavExpandOrOpen,
    TogglePinSelected,
    ToggleUnreadFilter,
    JumpNextUnread,
    JumpPrevUnread,
    ToggleLastConversation,
    HistoryBack,
    HistoryForward,
    PrevConversation,
    NextConversation,
    OpenConversationKey {
        conv_key: String,
        focus_composer: bool,
    },

    // Message selection & scrolling
    SelectMessage {
        scope: MessageScope,
        msg_index: usize,
    },
    MoveUp,
    MoveDown,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    MouseScroll {
        delta: i32,
        column: u16,
        row: u16,
    },
    Activate,

    // Composer editing & clipboard
    OpenComposer,
    ToggleSubject,
    FocusSubject,
    SendMessage,
    InsertNewline,
    Backspace,
    DeleteWordBackward,
    DeleteForward,
    Input(char),
    Paste(String),
    ClearComposer,
    ClearPendingImages,
    Tab,
    ComposerEscape,
    CursorLeft,
    CursorRight,
    CursorUp,
    CursorDown,
    CursorHome,
    CursorEnd,
    CursorDocStart,
    CursorDocEnd,
    SelectLeft,
    SelectRight,
    SelectUp,
    SelectDown,
    SelectHome,
    SelectEnd,
    SelectDocStart,
    SelectDocEnd,
    SelectAll,
    Copy,
    Cut,
    PasteClipboard,
    Undo,
    Redo,
    AcceptMention,
    SelectMention(usize),
    OpenExternalEditor,

    // Threads & replies
    OpenThreadForSelected,
    OpenThread {
        msg_index: usize,
    },
    CloseThreadDrawer,
    ToggleComposerTarget,
    ReplyToSelected,
    ReplyTo {
        scope: MessageScope,
        msg_index: usize,
    },
    CancelReply,

    // Copy & delete
    CopyMessage {
        scope: MessageScope,
        msg_index: usize,
    },
    CopySelectedMessage,
    CopyFocused,
    CopyText(String),
    RequestDeleteMessage {
        scope: MessageScope,
        msg_index: usize,
    },
    RequestDeleteSelectedMessage,
    DeleteMessage {
        conv_key: String,
        msg_id: String,
        parent_msg_id: Option<String>,
    },

    // Reactions
    OpenReactionsForSelected,
    OpenReactions {
        scope: MessageScope,
        msg_index: usize,
    },
    ToggleReaction {
        scope: MessageScope,
        msg_index: usize,
        reaction_type: String,
    },
    ToggleReactionByIndex {
        scope: MessageScope,
        msg_index: usize,
        reaction_index: usize,
    },

    // Attachments & files
    DownloadAttachment {
        scope: MessageScope,
        msg_index: usize,
        attach_index: usize,
    },
    DownloadLatest,
    OpenSendFile,
    OpenFilePicker,
    AttachPickedFile(String),
    SendAttachmentPath {
        path: String,
        caption: String,
    },
    AttachmentNext,
    AttachmentPrev,

    // Links & href visibility
    OpenLinks,
    OpenUrl(String),
    ToggleShowHrefs,

    // Search, sync & history
    Search(String),
    OpenSearch,
    OpenSearchWithQuery(String),
    JumpToSearchHit(usize),
    RefreshAll,
    SyncHistory,
    LoadOlder,
    MarkActiveRead,

    // People, chats & presence
    SearchUsers(String),
    CreateChat {
        peers: Vec<String>,
        topic: Option<String>,
    },
    SetPresence {
        availability: String,
        activity: String,
    },

    // Overlays & mouse
    OpenOverlay(OverlayKind),
    CloseOverlay,
    OpenContextMenuAt {
        column: u16,
        row: u16,
    },
    ContextMenuCurrent,
    OpenConfirm {
        message: String,
        on_confirm: Option<Box<Action>>,
    },
    OverlaySelect(usize),
    OverlaySubmit,
    Click {
        column: u16,
        row: u16,
    },
    ShowToast(String),

    // Inline Images, Interactive HTML Preview & Updater
    ToggleInlineImages,
    OpenImagePreview,
    LoadPreviewImage,
    CycleImageProtocol,
    DownloadPreviewImage,
    OpenHtmlPreview,
    OpenHtmlPreviewForMessage {
        scope: MessageScope,
        msg_index: usize,
    },
    ServeHtmlPreview,
    CheckForUpdates,
    ApplySelfUpdate,
}

/// Dispatches an `Action`, mutating `app` and `ui`.
/// Returns `true` when the application should exit.
pub fn dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    if matches!(action, Action::Quit) {
        app.stash_draft(ui);
        return true;
    }

    if let Action::Click { column, row } = action {
        return handle_click(column, row, app, ui);
    }

    if let Action::OpenContextMenuAt { column, row } = action {
        open_context_menu_at(column, row, app, ui);
        return false;
    }

    // If an overlay is open, let it handle actions first.
    if let Some(mut top) = ui.overlays.pop() {
        let outcome = top.handle(&action, app, ui);
        match outcome {
            OverlayOutcome::Handled => {
                ui.overlays.push(top);
                return false;
            }
            OverlayOutcome::Close => {
                return false;
            }
            OverlayOutcome::Dispatch(next) => {
                ui.overlays.push(top);
                return dispatch(next, app, ui);
            }
            OverlayOutcome::CloseAndDispatch(next) => {
                return dispatch(next, app, ui);
            }
            OverlayOutcome::NotHandled => {
                ui.overlays.push(top);
            }
        }
    }

    // Mention popup interception while typing in the composer.
    if matches!(ui.focus(), Focus::Composer) && ui.composer.mention_active() {
        match action {
            Action::MoveUp | Action::CursorUp => {
                ui.composer.mention_prev();
                return false;
            }
            Action::MoveDown | Action::CursorDown => {
                ui.composer.mention_next();
                return false;
            }
            Action::AcceptMention | Action::Tab | Action::Activate => {
                ui.composer.accept_mention();
                return false;
            }
            Action::ComposerEscape | Action::CloseOverlay => {
                ui.composer.close_mentions();
                return false;
            }
            _ => {}
        }
    }

    match action {
        Action::Quit => unreachable!(),
        Action::QuitConfirm => {
            if ui.composer.is_empty() {
                app.stash_draft(ui);
                return true;
            }
            ui.close_overlays_of(OverlayKind::Confirm);
            ui.push_overlay(Overlay::confirm(
                "Discard unsent draft and quit?",
                Some(Action::Quit),
            ));
        }
        Action::Noop => {}

        // Focus & layout
        Action::Focus(focus) => {
            ui.set_focus(focus);
        }
        Action::NextFocus => cycle_focus(app, ui, true),
        Action::PrevFocus => cycle_focus(app, ui, false),
        Action::ToggleNavRail => {
            ui.show_nav_rail = !ui.show_nav_rail;
            app.config.show_nav_rail = ui.show_nav_rail;
            let _ = app.config.save();
        }
        Action::ToggleAutoThreadPane => {
            ui.auto_thread_pane = !ui.auto_thread_pane;
            app.config.auto_thread_pane = ui.auto_thread_pane;
            let _ = app.config.save();
            ui.info(if ui.auto_thread_pane {
                "Auto thread pane: On"
            } else {
                "Auto thread pane: Off"
            });
        }
        Action::ToggleNarrowPane => {
            ui.narrow_pane = match ui.narrow_pane {
                NarrowPane::List => NarrowPane::Chat,
                NarrowPane::Chat => {
                    if ui.thread_drawer_open && app.active_thread.is_some() {
                        NarrowPane::Thread
                    } else {
                        NarrowPane::List
                    }
                }
                NarrowPane::Thread => NarrowPane::Chat,
            };
            match ui.narrow_pane {
                NarrowPane::List => ui.set_focus(Focus::Nav),
                NarrowPane::Chat => ui.set_focus(Focus::Chat),
                NarrowPane::Thread => ui.set_focus(Focus::Thread),
            }
        }
        Action::SelectNarrowPane(pane) => match pane {
            NarrowPane::List => {
                ui.narrow_pane = NarrowPane::List;
                ui.set_focus(Focus::Nav);
            }
            NarrowPane::Chat => {
                ui.narrow_pane = NarrowPane::Chat;
                ui.set_focus(Focus::Composer);
            }
            NarrowPane::Thread => {
                if ui.thread_drawer_open && app.active_thread.is_some() {
                    ui.narrow_pane = NarrowPane::Thread;
                    ui.set_focus(Focus::Thread);
                }
            }
        },
        Action::ToggleNotify => {
            ui.notify = !ui.notify;
            app.config.notify = ui.notify;
            let _ = app.config.save();
            ui.info(if ui.notify {
                "Desktop notifications: On"
            } else {
                "Desktop notifications: Off"
            });
        }
        Action::ToggleDensity => {
            ui.density = ui.density.toggle();
            app.config.density = ui.density;
            let _ = app.config.save();
            ui.info(format!("Message density: {}", ui.density.label()));
        }
        Action::CycleTheme => {
            ui.theme_preset = ui.theme_preset.next();
            ui.theme = super::theme::Theme::detect(ui.theme_preset);
            app.config.theme = ui.theme_preset;
            let _ = app.config.save();
            ui.info(format!("Theme: {}", ui.theme_preset.label()));
        }
        Action::ShowDownloadDir => {
            ui.info(format!("Downloads folder: {}", app.download_dir.display()));
        }

        // Navigation pane
        Action::SelectNavTab(tab) => {
            ui.show_nav_rail = true;
            ui.active_nav_tab = tab.min(TAB_COUNT - 1);
            let rows = nav_pane::nav_rows_cached(app, ui);
            ui.nav_selected = first_selectable_nav_row(&rows).unwrap_or(0);
            ui.narrow_pane = NarrowPane::List;
            ui.set_focus(Focus::Nav);
            app.ensure_tab_domain_activated(ui, ui.active_nav_tab);
        }
        Action::SelectNavRow(idx) => {
            app.select_nav_row(ui, idx);
        }
        Action::NavFirst => {
            let rows = nav_pane::nav_rows_cached(app, ui);
            if let Some(first) = first_selectable_nav_row(&rows) {
                ui.nav_selected = first;
            }
        }
        Action::NavLast => {
            let rows = nav_pane::nav_rows_cached(app, ui);
            if let Some(last) = last_selectable_nav_row(&rows) {
                ui.nav_selected = last;
                maybe_load_older_chats(app, ui);
            }
        }
        Action::NavCollapseOrParent => {
            let rows = nav_pane::nav_rows_cached(app, ui);
            match rows.get(ui.nav_selected) {
                Some(NavRow::TeamHeader {
                    team_id,
                    collapsed: false,
                    ..
                }) => {
                    ui.collapsed_team_ids.insert(team_id.clone());
                }
                Some(NavRow::TeamChannel { .. }) => {
                    if let Some(parent_idx) = rows[..ui.nav_selected]
                        .iter()
                        .rposition(|r| matches!(r, NavRow::TeamHeader { .. }))
                    {
                        ui.nav_selected = parent_idx;
                    }
                }
                _ => {}
            }
        }
        Action::NavExpandOrOpen => {
            let rows = nav_pane::nav_rows_cached(app, ui);
            match rows.get(ui.nav_selected) {
                Some(NavRow::TeamHeader {
                    team_id,
                    collapsed: true,
                    ..
                }) => {
                    ui.collapsed_team_ids.remove(team_id);
                }
                Some(
                    NavRow::Chat { .. }
                    | NavRow::TeamChannel { .. }
                    | NavRow::ActivityMention { .. },
                ) => {
                    app.select_nav_row(ui, ui.nav_selected);
                }
                _ => {}
            }
        }
        Action::TogglePinSelected => {
            let conv_key = match nav_pane::nav_row_at(app, ui, ui.nav_selected) {
                Some(NavRow::Chat { conv_key, .. })
                | Some(NavRow::TeamChannel { conv_key, .. }) => Some(conv_key),
                _ => None,
            };
            if let Some(key) = conv_key {
                let pinned_now = if ui.pinned_convs.remove(&key) {
                    false
                } else {
                    ui.pinned_convs.insert(key.clone());
                    true
                };
                app.config.pinned_convs = ui.pinned_convs.iter().cloned().collect();
                app.config.pinned_convs.sort();
                let _ = app.config.save();
                if let Some(conv) = app.conversations.iter_mut().find(|c| c.conv_key == key) {
                    conv.pinned = pinned_now;
                }
                ui.info(if pinned_now {
                    "Pinned conversation"
                } else {
                    "Unpinned conversation"
                });
            }
        }
        Action::ToggleUnreadFilter => {
            ui.nav_unread_only = !ui.nav_unread_only;
            let rows = nav_pane::nav_rows_cached(app, ui);
            ui.nav_selected = first_selectable_nav_row(&rows).unwrap_or(0);
            ui.info(if ui.nav_unread_only {
                "Filter: Unread only"
            } else {
                "Filter: All conversations"
            });
        }
        Action::JumpNextUnread => {
            jump_unread(app, ui, true);
        }
        Action::JumpPrevUnread => {
            jump_unread(app, ui, false);
        }
        Action::ToggleLastConversation => {
            if let Some(last_key) = ui.last_conv_key.clone() {
                if Some(&last_key) != app.active_conv_key.as_ref() {
                    if let Some(target) = app.target_for_conv_key(&last_key) {
                        app.select_conversation(ui, target);
                    }
                }
            } else {
                ui.info("No previous conversation");
            }
        }
        Action::HistoryBack => {
            app.history_back(ui);
        }
        Action::HistoryForward => {
            app.history_forward(ui);
        }
        Action::PrevConversation => {
            step_active_nav_conversation(app, ui, -1);
        }
        Action::NextConversation => {
            step_active_nav_conversation(app, ui, 1);
        }
        Action::OpenConversationKey {
            conv_key,
            focus_composer,
        } => {
            if let Some(target) = app.target_for_conv_key(&conv_key) {
                app.select_conversation(ui, target);
                if !focus_composer {
                    ui.set_focus(Focus::Chat);
                }
            }
        }

        // Message selection & scrolling
        Action::SelectMessage { scope, msg_index } => match scope {
            MessageScope::Chat => {
                if msg_index < app.messages.len() {
                    ui.chat_scroll.selected_idx = Some(msg_index);
                    ui.set_focus(Focus::Chat);
                }
            }
            MessageScope::ThreadRoot | MessageScope::ThreadReply => {
                ui.thread_scroll.selected_idx = Some(match scope {
                    MessageScope::ThreadRoot => 0,
                    MessageScope::ThreadReply => msg_index + 1,
                    MessageScope::Chat => unreachable!(),
                });
                ui.set_focus(Focus::Thread);
            }
        },
        Action::MoveUp => move_selection(app, ui, -1),
        Action::MoveDown => move_selection(app, ui, 1),
        Action::ScrollUp => {
            if ui.focus() == Focus::Composer {
                scroll_active_stream_from_composer(app, ui, -3);
            } else {
                scroll_focused_and_autoload(app, ui, -3);
            }
        }
        Action::ScrollDown => {
            if ui.focus() == Focus::Composer {
                scroll_active_stream_from_composer(app, ui, 3);
            } else {
                scroll_focused_and_autoload(app, ui, 3);
            }
        }
        Action::ScrollPageUp => {
            let half = active_stream_half_page(ui);
            if ui.focus() == Focus::Composer {
                scroll_active_stream_from_composer(app, ui, -half);
            } else {
                scroll_focused_and_autoload(app, ui, -half);
            }
        }
        Action::ScrollPageDown => {
            let half = active_stream_half_page(ui);
            if ui.focus() == Focus::Composer {
                scroll_active_stream_from_composer(app, ui, half);
            } else {
                scroll_focused_and_autoload(app, ui, half);
            }
        }
        Action::ScrollTop => match ui.focus() {
            Focus::Nav => {
                let _ = dispatch(Action::NavFirst, app, ui);
            }
            Focus::Thread => ui.thread_scroll.top(),
            _ => {
                ui.pending_append_anchor = None;
                ui.chat_scroll.top();
                if !app.oldest_reached && !app.loading_older && app.active_conv_key.is_some() {
                    app.load_older_auto(ui);
                }
            }
        },
        Action::ScrollBottom => match ui.focus() {
            Focus::Nav => {
                let _ = dispatch(Action::NavLast, app, ui);
            }
            Focus::Thread => ui.thread_scroll.bottom(),
            _ => {
                ui.pending_append_anchor = None;
                ui.chat_scroll.bottom();
            }
        },
        Action::MouseScroll { delta, column, row } => {
            handle_mouse_scroll(app, ui, delta, column, row);
        }
        Action::Activate => handle_activate(app, ui),

        // Composer
        Action::OpenComposer => {
            ui.set_focus(Focus::Composer);
        }
        Action::ToggleSubject => {
            let is_channel = app
                .active_target
                .as_ref()
                .is_some_and(ConversationTarget::is_channel);
            if !is_channel || ui.composer_target_thread {
                ui.info("Subject is only available for channel root posts");
            } else {
                ui.show_subject = !ui.show_subject;
                ui.composer.subject_focused = ui.show_subject;
                if ui.show_subject {
                    ui.set_focus(Focus::Composer);
                }
            }
        }
        Action::FocusSubject => {
            ui.show_subject = true;
            ui.composer.subject_focused = !ui.composer.subject_focused;
            ui.set_focus(Focus::Composer);
        }
        Action::SendMessage => {
            app.send_composer(ui);
        }
        Action::InsertNewline => {
            if ui.composer.subject_focused {
                ui.composer.subject_focused = false;
            } else {
                ui.composer.insert_char('\n');
                ui.composer.reference_mentions(&app.members);
            }
        }
        Action::Backspace => {
            ui.composer.backspace();
            ui.composer.reference_mentions(&app.members);
        }
        Action::DeleteWordBackward => {
            ui.composer.delete_word_backward();
            ui.composer.reference_mentions(&app.members);
        }
        Action::DeleteForward => {
            ui.composer.delete_forward();
            ui.composer.reference_mentions(&app.members);
        }
        Action::Input(ch) => {
            if !matches!(ui.focus(), Focus::Composer) {
                ui.set_focus(Focus::Composer);
            }
            ui.composer.insert_char(ch);
            ui.composer.reference_mentions(&app.members);
        }
        Action::Paste(text) => {
            if !matches!(ui.focus(), Focus::Composer) {
                ui.set_focus(Focus::Composer);
            }
            ui.composer.insert_str(&text);
            ui.composer.reference_mentions(&app.members);
        }
        Action::ClearComposer => {
            app.clear_current_draft(ui);
        }
        Action::ClearPendingImages => {
            ui.composer.clear_pending_images();
            ui.info("Cleared pending inline images");
        }
        Action::Tab => {
            if ui.composer.subject_focused {
                ui.composer.subject_focused = false;
            } else {
                ui.composer.insert_str("    ");
            }
        }
        Action::ComposerEscape => {
            if ui.composer.mention_active() {
                ui.composer.close_mentions();
            } else if ui.composer.reply().is_some() {
                ui.composer.clear_reply();
            } else if ui.composer.subject_focused {
                ui.composer.subject_focused = false;
            } else {
                ui.toggle_composer_focus(false);
            }
        }
        Action::CursorLeft => ui.composer.move_left(false),
        Action::CursorRight => ui.composer.move_right(false),
        Action::CursorUp => {
            if ui.focus() == Focus::Composer
                && !ui.composer.popup.is_open()
                && !ui.composer.subject_focused
                && ui.composer.has_no_text()
            {
                scroll_active_stream_from_composer(app, ui, -3);
            } else {
                ui.composer.move_up(false);
            }
        }
        Action::CursorDown => {
            if ui.focus() == Focus::Composer
                && !ui.composer.popup.is_open()
                && !ui.composer.subject_focused
                && ui.composer.has_no_text()
            {
                scroll_active_stream_from_composer(app, ui, 3);
            } else {
                ui.composer.move_down(false);
            }
        }
        Action::CursorHome => ui.composer.move_home(false),
        Action::CursorEnd => ui.composer.move_end(false),
        Action::CursorDocStart => ui.composer.move_doc_start(false),
        Action::CursorDocEnd => ui.composer.move_doc_end(false),
        Action::SelectLeft => ui.composer.move_left(true),
        Action::SelectRight => ui.composer.move_right(true),
        Action::SelectUp => ui.composer.move_up(true),
        Action::SelectDown => ui.composer.move_down(true),
        Action::SelectHome => ui.composer.move_home(true),
        Action::SelectEnd => ui.composer.move_end(true),
        Action::SelectDocStart => ui.composer.move_doc_start(true),
        Action::SelectDocEnd => ui.composer.move_doc_end(true),
        Action::SelectAll => ui.composer.select_all(),
        Action::Copy => {
            if matches!(ui.focus(), Focus::Composer) {
                let text = ui
                    .composer
                    .selected_text()
                    .unwrap_or_else(|| ui.composer.text())
                    .to_string();
                copy_to_clipboard(&text, ui);
            } else {
                let _ = dispatch(Action::CopySelectedMessage, app, ui);
            }
        }
        Action::Cut => {
            if let Some(sel) = ui.composer.selected_text().map(str::to_string) {
                ui.composer.delete_selection();
                copy_to_clipboard(&sel, ui);
            }
        }
        Action::PasteClipboard => {
            paste_clipboard(app, ui);
        }
        Action::Undo => {
            ui.composer.undo();
        }
        Action::Redo => {
            ui.composer.redo();
        }
        Action::AcceptMention => {
            ui.composer.accept_mention();
        }
        Action::SelectMention(index) => {
            if ui.composer.mention_active() && index < ui.composer.popup.candidates.len() {
                ui.composer.popup.selected = index;
                ui.composer.accept_mention();
                ui.set_focus(Focus::Composer);
            }
        }
        Action::OpenExternalEditor => {
            ui.set_focus(Focus::Composer);
            ui.editor_requested = true;
        }

        // Threads & replies
        Action::OpenThreadForSelected => {
            if let Some(idx) = selected_chat_index(app, ui) {
                app.open_thread(ui, idx);
            }
        }
        Action::OpenThread { msg_index } => {
            app.open_thread(ui, msg_index);
        }
        Action::CloseThreadDrawer => {
            app.close_thread(ui);
        }
        Action::ToggleComposerTarget => {
            if app.active_thread.is_some() {
                app.stash_and_reset_draft(ui);
                ui.composer_target_thread = !ui.composer_target_thread;
                app.restore_draft(ui);
            } else {
                ui.composer_target_thread = false;
                ui.info("No open thread; composing in main conversation");
            }
        }
        Action::ReplyToSelected => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                let _ = dispatch(Action::ReplyTo { scope, msg_index }, app, ui);
            }
        }
        Action::ReplyTo { scope, msg_index } => {
            if matches!(scope, MessageScope::Chat)
                && app
                    .active_target
                    .as_ref()
                    .is_some_and(ConversationTarget::is_channel)
            {
                app.open_thread(ui, msg_index);
                ui.set_focus(Focus::Composer);
            } else if let Some(msg) = app.message_at(scope, msg_index) {
                ui.composer.set_reply(ReplyPreview {
                    msg_id: msg.msg_id.clone(),
                    author: msg.author_name.clone(),
                    text: msg.body_plain.chars().take(80).collect(),
                });
                ui.set_focus(Focus::Composer);
            }
        }
        Action::CancelReply => {
            ui.composer.clear_reply();
        }

        // Copy & delete
        Action::CopyMessage { scope, msg_index } => {
            if let Some(msg) = app.message_at(scope, msg_index) {
                let text = if let Some(sub) = &msg.subject {
                    format!("{sub}\n{}", msg.body_plain)
                } else {
                    msg.body_plain.clone()
                };
                copy_to_clipboard(&text, ui);
            }
        }
        Action::CopySelectedMessage | Action::CopyFocused => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                let _ = dispatch(Action::CopyMessage { scope, msg_index }, app, ui);
            }
        }
        Action::CopyText(text) => {
            copy_to_clipboard(&text, ui);
        }
        Action::RequestDeleteMessage { scope, msg_index } => {
            if let (Some(conv_key), Some(msg)) =
                (app.active_conv_key.clone(), app.message_at(scope, msg_index))
            {
                if !msg.is_me {
                    ui.info("You can only delete your own messages");
                    return false;
                }
                let parent_msg_id = match scope {
                    MessageScope::ThreadReply => {
                        app.active_thread.as_ref().map(|t| t.root.msg_id.clone())
                    }
                    _ => msg.parent_msg_id.clone(),
                };
                let preview: String = msg.body_plain.chars().take(40).collect();
                let confirm = Action::DeleteMessage {
                    conv_key,
                    msg_id: msg.msg_id.clone(),
                    parent_msg_id,
                };
                ui.push_overlay(Overlay::confirm(
                    format!("Delete this message?\n\"{preview}\""),
                    Some(confirm),
                ));
            }
        }
        Action::RequestDeleteSelectedMessage => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                let _ = dispatch(
                    Action::RequestDeleteMessage { scope, msg_index },
                    app,
                    ui,
                );
            }
        }
        Action::DeleteMessage {
            conv_key,
            msg_id,
            parent_msg_id,
        } => {
            app.delete_message(ui, conv_key, msg_id, parent_msg_id);
        }

        // Reactions
        Action::OpenReactionsForSelected => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                ui.push_overlay(Overlay::reactions(scope, msg_index));
            }
        }
        Action::OpenReactions { scope, msg_index } => {
            ui.push_overlay(Overlay::reactions(scope, msg_index));
        }
        Action::ToggleReaction {
            scope,
            msg_index,
            reaction_type,
        } => {
            app.toggle_reaction(ui, scope, msg_index, reaction_type);
        }
        Action::ToggleReactionByIndex {
            scope,
            msg_index,
            reaction_index,
        } => {
            let reaction_type = app
                .message_at(scope, msg_index)
                .and_then(|m| m.reactions.get(reaction_index))
                .map(|r| r.reaction_type.clone());
            if let Some(rt) = reaction_type {
                app.toggle_reaction(ui, scope, msg_index, rt);
            }
        }

        // Attachments
        Action::DownloadAttachment {
            scope,
            msg_index,
            attach_index,
        } => {
            app.download_attachment(ui, scope, msg_index, attach_index);
        }
        Action::DownloadLatest => {
            if let Some((scope, msg_index)) = selected_message_scope_and_index(app, ui) {
                if app
                    .message_at(scope, msg_index)
                    .is_some_and(|m| !m.effective_attachments().is_empty())
                {
                    app.download_attachment(ui, scope, msg_index, 0);
                    return false;
                }
            }
            if let Some((idx, _)) = app
                .messages
                .iter()
                .enumerate()
                .rev()
                .find(|(_, m)| !m.effective_attachments().is_empty())
            {
                app.download_attachment(ui, MessageScope::Chat, idx, 0);
            } else {
                ui.info("No attachments to download");
            }
        }
        Action::OpenSendFile => {
            open_overlay(OverlayKind::PathInput, app, ui);
        }
        Action::OpenFilePicker => {
            open_overlay(OverlayKind::FilePicker, app, ui);
        }
        Action::AttachPickedFile(path) => {
            let caption = ui.composer.text().trim().to_string();
            if !caption.is_empty() {
                app.clear_current_draft(ui);
            }
            app.send_file(ui, path, caption);
        }
        Action::SendAttachmentPath { path, caption } => {
            app.send_file(ui, path, caption);
        }
        Action::AttachmentNext => {
            ui.attach_index = ui.attach_index.saturating_add(1);
        }
        Action::AttachmentPrev => {
            ui.attach_index = ui.attach_index.saturating_sub(1);
        }

        // Links & href visibility
        Action::OpenLinks => {
            open_links_overlay(app, ui);
        }
        Action::OpenUrl(url) => match open::that_detached(&url) {
            Ok(()) => ui.info(format!("Opened link: {url}")),
            Err(err) => ui.error(format!("Failed to open link: {err}")),
        },
        Action::ToggleShowHrefs => {
            ui.show_hrefs = !ui.show_hrefs;
            ui.info(if ui.show_hrefs {
                "Link URLs (href): Shown"
            } else {
                "Link URLs (href): Hidden"
            });
        }

        // Search, sync & history
        Action::Search(query) => {
            app.search_messages(ui, query);
        }
        Action::OpenSearch => {
            open_overlay(OverlayKind::Search, app, ui);
        }
        Action::OpenSearchWithQuery(query) => {
            ui.close_overlays_of(OverlayKind::Search);
            let mut search = SearchOverlay::default();
            search.query = query.clone();
            ui.push_overlay(Overlay::Search(search));
            if !query.trim().is_empty() {
                app.search_messages(ui, query);
            }
        }
        Action::JumpToSearchHit(hit_idx) => {
            if let Some(hit) = app.search_results.get(hit_idx).cloned() {
                if let Some(target) = ConversationTarget::from_conv_key(&hit.conv_key) {
                    app.select_conversation(ui, target);
                    if let Some(root) = hit.parent_msg_id {
                        app.open_thread_by_id(ui, root);
                    } else if let Some(idx) =
                        app.messages.iter().position(|m| m.msg_id == hit.msg_id)
                    {
                        ui.chat_scroll.selected_idx = Some(idx);
                    }
                }
            }
        }
        Action::RefreshAll => {
            app.sync_ctrl.request(SyncCommand::RefreshConversations);
            app.sync_ctrl.request(SyncCommand::SyncActiveNow);
            ui.info("Refreshing Teams conversations...");
        }
        Action::SyncHistory => {
            if matches!(ui.focus(), Focus::Thread) && app.active_thread.is_some() {
                app.refresh_active_thread(ui);
            } else {
                app.load_older_with_depth(ui, 150);
            }
        }
        Action::LoadOlder => {
            app.load_older(ui);
        }
        Action::MarkActiveRead => {
            if let Some(conv_key) = app.active_conv_key.clone() {
                let now_ts = Utc::now().to_rfc3339();
                if let Some(conv) = app.conversations.iter_mut().find(|c| c.conv_key == conv_key) {
                    conv.unread_count = 0;
                    conv.last_read_at = Some(now_ts.clone());
                }
                let _ = app.storage.mark_conversation_read(&conv_key, &now_ts);
                if tokio::runtime::Handle::try_current().is_ok() {
                    if let Some(ConversationTarget::Chat { chat_id }) = app.active_target.clone() {
                        let graph = app.graph.clone();
                        tokio::spawn(async move {
                            let _ = graph.mark_chat_read(&chat_id).await;
                        });
                    }
                }
                ui.info("Marked as read");
            }
        }

        // People & presence
        Action::SearchUsers(query) => {
            app.search_users(ui, query);
        }
        Action::CreateChat { peers, topic } => {
            app.create_chat(ui, peers, topic);
        }
        Action::SetPresence {
            availability,
            activity,
        } => {
            app.set_presence(ui, availability, activity);
        }

        // Overlays
        Action::OpenOverlay(kind) => {
            open_overlay(kind, app, ui);
        }
        Action::CloseOverlay => {
            if ui.close_top_overlay().is_none()
                && ui.thread_drawer_open
                && matches!(ui.focus(), Focus::Thread)
            {
                app.close_thread(ui);
            }
        }
        Action::ContextMenuCurrent => {
            let ctx = match ui.focus() {
                Focus::Nav => ContextTarget::NavRow(ui.nav_selected),
                Focus::Chat => {
                    if let Some(idx) = selected_chat_index(app, ui) {
                        ContextTarget::Message {
                            scope: MessageScope::Chat,
                            msg_index: idx,
                        }
                    } else {
                        ContextTarget::Global
                    }
                }
                Focus::Thread => {
                    if let Some((scope, idx)) = selected_thread_scope_and_index(app, ui) {
                        ContextTarget::Message {
                            scope,
                            msg_index: idx,
                        }
                    } else {
                        ContextTarget::Global
                    }
                }
                Focus::Composer => ContextTarget::Composer,
                Focus::Overlay => ContextTarget::Global,
            };
            let menu = ContextMenu::build(ctx, (12, 6), app, ui);
            ui.push_overlay(Overlay::context_menu(menu));
        }
        Action::OpenConfirm {
            message,
            on_confirm,
        } => {
            ui.push_overlay(Overlay::confirm(message, on_confirm.map(|b| *b)));
        }
        Action::OverlaySelect(index) => {
            if let Some(top) = ui.overlays.last_mut() {
                top.select(index);
            }
        }
        Action::OverlaySubmit => {}
        Action::Click { .. } | Action::OpenContextMenuAt { .. } => unreachable!(),
        Action::ShowToast(msg) => {
            ui.info(msg);
        }

        // Inline Images & Updater
        Action::ToggleInlineImages => {
            ui.inline_images = !ui.inline_images;
            app.config.inline_images = ui.inline_images;
            let _ = app.config.save();
            ui.info(if ui.inline_images {
                "Inline image previews: On"
            } else {
                "Inline image previews: Off"
            });
        }
        Action::OpenImagePreview => {
            open_image_preview(app, ui);
        }
        Action::LoadPreviewImage => {
            let dims = (ui.term_width, ui.term_height);
            let target_and_dims = ui.overlays.last().and_then(|top| {
                if let Overlay::ImagePreview(preview) = top {
                    preview
                        .current()
                        .cloned()
                        .map(|t| (t, preview.target_dimensions(dims.0, dims.1)))
                } else {
                    None
                }
            });
            if let Some((target, (cols, rows))) = target_and_dims {
                let key = AppState::preview_image_cache_key(
                    target.owner_id(),
                    target.att_id(),
                    cols,
                    rows,
                    ui.graphics_caps.protocol,
                );
                app.clear_image_error(&key);
                app.request_preview_image(
                    &target,
                    cols,
                    rows,
                    ui.graphics_caps,
                    theme_bg_rgb(ui.theme.bg_elevated),
                );
            }
        }
        Action::CycleImageProtocol => {
            ui.graphics_caps.protocol = ui.graphics_caps.protocol.next_manual();
            ui.info(format!("Image protocol: {}", ui.graphics_caps.protocol.label()));
            if let Some(Overlay::ImagePreview(preview)) = ui.overlays.last_mut() {
                preview.error = None;
            }
            let _ = dispatch(Action::LoadPreviewImage, app, ui);
        }
        Action::DownloadPreviewImage => {
            let current = ui.overlays.last().and_then(|top| {
                if let Overlay::ImagePreview(preview) = top {
                    preview.current().cloned()
                } else {
                    None
                }
            });
            if let Some(super::overlays::image_preview::PreviewTarget::Teams { attachment, .. }) =
                current
            {
                let graph = app.graph.clone();
                let dir = app.download_dir.clone();
                let tx = app.ui_tx.clone();
                let label = format!(
                    "Save image: {}",
                    attachment.name.as_deref().unwrap_or(&attachment.id)
                );
                ui.tasks.begin(label.clone());
                tokio::spawn(async move {
                    match graph.download_attachment(&attachment, &dir).await {
                        Ok(file) => {
                            let _ = tx.send(super::app::UiMsg::TaskDone {
                                label,
                                error: None,
                                toast: Some((
                                    format!("Saved: {}", file.path.display()),
                                    super::toast::ToastKind::Success,
                                )),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(super::app::UiMsg::TaskDone {
                                label,
                                error: Some(err.to_string()),
                                toast: Some((
                                    format!("Save failed: {err}"),
                                    super::toast::ToastKind::Error,
                                )),
                            });
                        }
                    }
                });
            }
        }
        Action::OpenHtmlPreview => {
            open_html_preview(app, ui);
        }
        Action::OpenHtmlPreviewForMessage { scope, msg_index } => {
            open_html_preview_for_message(scope, msg_index, app, ui);
        }
        Action::ServeHtmlPreview => {
            serve_active_html_preview(app, ui);
        }
        Action::CheckForUpdates => {
            app.check_for_updates(ui, true, false);
        }
        Action::ApplySelfUpdate => {
            app.check_for_updates(ui, false, false);
        }
    }

    false
}

fn open_overlay(kind: OverlayKind, app: &mut AppState, ui: &mut UiState) {
    if kind.is_singleton() {
        ui.close_overlays_of(kind);
    }
    let overlay = match kind {
        OverlayKind::ContextMenu => {
            Overlay::context_menu(ContextMenu::build(ContextTarget::Global, (10, 5), app, ui))
        }
        OverlayKind::CommandPalette => Overlay::command_palette(),
        OverlayKind::Settings => Overlay::settings(),
        OverlayKind::Search => Overlay::Search(SearchOverlay::default()),
        OverlayKind::Presence => Overlay::presence(),
        OverlayKind::Reactions => {
            let (scope, idx) =
                selected_message_scope_and_index(app, ui).unwrap_or((MessageScope::Chat, 0));
            Overlay::reactions(scope, idx)
        }
        OverlayKind::Attachments => Overlay::attachments(),
        OverlayKind::PathInput => Overlay::path_input(),
        OverlayKind::FilePicker => Overlay::file_picker(),
        OverlayKind::Links => {
            open_links_overlay(app, ui);
            return;
        }
        OverlayKind::NewChat => Overlay::new_chat(),
        OverlayKind::RoomInfo => Overlay::room_info(),
        OverlayKind::ImagePreview => {
            open_image_preview(app, ui);
            return;
        }
        OverlayKind::HtmlPreview => {
            open_html_preview(app, ui);
            return;
        }
        OverlayKind::Help => Overlay::help(),
        OverlayKind::Confirm => Overlay::confirm("Confirm?", None),
    };
    ui.push_overlay(overlay);
}

fn theme_bg_rgb(color: ratatui::style::Color) -> (u8, u8, u8) {
    match color {
        ratatui::style::Color::Rgb(r, g, b) => (r, g, b),
        _ => (37, 42, 61),
    }
}

fn push_image_preview(preview: ImagePreviewOverlay, app: &mut AppState, ui: &mut UiState) {
    ui.close_overlays_of(OverlayKind::ImagePreview);
    if let Some(target) = preview.current().cloned() {
        let (cols, rows) = preview.target_dimensions(ui.term_width, ui.term_height);
        let key = AppState::preview_image_cache_key(
            target.owner_id(),
            target.att_id(),
            cols,
            rows,
            ui.graphics_caps.protocol,
        );
        app.clear_image_error(&key);
        app.request_preview_image(
            &target,
            cols,
            rows,
            ui.graphics_caps,
            theme_bg_rgb(ui.theme.bg_elevated),
        );
    }
    ui.push_overlay(Overlay::ImagePreview(preview));
}

fn open_image_preview(app: &mut AppState, ui: &mut UiState) {
    let in_thread_context =
        ui.focus() == Focus::Thread || (ui.composer_target_thread && ui.thread_drawer_open);
    let thread_preview = if in_thread_context {
        selected_thread_scope_and_index(app, ui)
            .and_then(|(scope, idx)| app.message_at(scope, idx))
            .and_then(|msg| ImagePreviewOverlay::from_teams_message(msg, Some(ui.attach_index)))
            .or_else(|| {
                app.active_thread.as_ref().and_then(|thread| {
                    thread
                        .replies
                        .iter()
                        .rev()
                        .chain(std::iter::once(&thread.root))
                        .find_map(|msg| ImagePreviewOverlay::from_teams_message(msg, None))
                })
            })
    } else {
        None
    };

    let preview_opt = thread_preview
        .or_else(|| {
            selected_message_scope_and_index(app, ui)
                .and_then(|(scope, idx)| app.message_at(scope, idx))
                .and_then(|msg| ImagePreviewOverlay::from_teams_message(msg, Some(ui.attach_index)))
        })
        .or_else(|| {
            app.messages
                .iter()
                .rev()
                .find_map(|msg| ImagePreviewOverlay::from_teams_message(msg, None))
        });

    let Some(preview) = preview_opt else {
        ui.info("No image attachments to preview");
        return;
    };

    push_image_preview(preview, app, ui);
}

fn open_html_preview(app: &mut AppState, ui: &mut UiState) {
    let in_thread_context =
        ui.focus() == Focus::Thread || (ui.composer_target_thread && ui.thread_drawer_open);
    let target = if in_thread_context {
        selected_thread_scope_and_index(app, ui)
            .or_else(|| app.active_thread.as_ref().map(|_| (MessageScope::ThreadRoot, 0)))
    } else {
        selected_message_scope_and_index(app, ui).or_else(|| {
            (!app.messages.is_empty()).then_some((MessageScope::Chat, app.messages.len() - 1))
        })
    };

    let Some((scope, msg_index)) = target else {
        ui.info("No message selected for HTML preview");
        return;
    };

    open_html_preview_for_message(scope, msg_index, app, ui);
}

fn open_html_preview_for_message(
    scope: MessageScope,
    msg_index: usize,
    app: &mut AppState,
    ui: &mut UiState,
) {
    let Some(msg) = app.message_at(scope, msg_index).cloned() else {
        ui.info("No message selected for HTML preview");
        return;
    };

    let eff = msg.effective_attachments();
    if let Some(html_att) = eff
        .iter()
        .find(|att| HtmlPreviewOverlay::is_html_attachment(att) && att.content_url.is_some())
        .cloned()
    {
        let body_trimmed = msg.body_html.trim();
        let body_is_empty_or_placeholder = body_trimmed.is_empty()
            || (body_trimmed.starts_with("<attachment")
                && body_trimmed.ends_with("</attachment>")
                && !body_trimmed.contains("<p")
                && !body_trimmed.contains("<div"));
        if body_is_empty_or_placeholder {
            app.fetch_html_attachment(ui, html_att);
            return;
        }
    }

    if let Some(overlay) = HtmlPreviewOverlay::from_teams_message(&msg) {
        ui.close_overlays_of(OverlayKind::HtmlPreview);
        ui.push_overlay(Overlay::HtmlPreview(overlay));
    } else {
        ui.info("No HTML content or Adaptive Card in this message");
    }
}

fn serve_active_html_preview(app: &mut AppState, ui: &mut UiState) {
    let payload = ui.overlays.last().and_then(|top| {
        if let Overlay::HtmlPreview(preview) = top {
            Some((preview.title().to_string(), preview.raw_html().to_string()))
        } else {
            None
        }
    });
    let Some((title, html)) = payload else {
        return;
    };
    app.serve_html_preview(ui, title, html);
}

fn open_links_overlay(app: &AppState, ui: &mut UiState) {
    let in_thread_context =
        ui.focus() == Focus::Thread || (ui.composer_target_thread && ui.thread_drawer_open);

    let (extracted, source_label) = if in_thread_context {
        let mut found = selected_thread_scope_and_index(app, ui)
            .and_then(|(scope, idx)| app.message_at(scope, idx))
            .map(|msg| (links::extract_links_from_message(msg), msg.author_name.clone()))
            .unwrap_or_else(|| (Vec::new(), String::new()));
        if found.0.is_empty() {
            if let Some(ref thread) = app.active_thread {
                let mut msgs = Vec::with_capacity(1 + thread.replies.len());
                msgs.push(&thread.root);
                msgs.extend(thread.replies.iter());
                found = (links::extract_links_from_messages(&msgs), "Thread".to_string());
            }
        }
        found
    } else {
        selected_message_scope_and_index(app, ui)
            .and_then(|(scope, idx)| app.message_at(scope, idx))
            .map(|msg| (links::extract_links_from_message(msg), msg.author_name.clone()))
            .unwrap_or_else(|| (Vec::new(), String::new()))
    };

    if extracted.is_empty() {
        ui.info("No links found in focused message");
        return;
    }

    ui.close_overlays_of(OverlayKind::Links);
    ui.push_overlay(Overlay::links(LinksOverlay::new(extracted, source_label)));
}

fn cycle_focus(app: &AppState, ui: &mut UiState, forward: bool) {
    let mut order = vec![Focus::Nav, Focus::Chat];
    if ui.thread_drawer_open && app.active_thread.is_some() {
        order.push(Focus::Thread);
    }
    order.push(Focus::Composer);

    let pos = order.iter().position(|f| *f == ui.focus).unwrap_or(0);
    let next_pos = if forward {
        (pos + 1) % order.len()
    } else {
        (pos + order.len() - 1) % order.len()
    };
    let next = order[next_pos];
    ui.set_focus(next);
    match next {
        Focus::Nav => ui.narrow_pane = NarrowPane::List,
        Focus::Chat => ui.narrow_pane = NarrowPane::Chat,
        Focus::Thread => ui.narrow_pane = NarrowPane::Thread,
        Focus::Composer | Focus::Overlay => {}
    }
}

fn selected_chat_index(app: &AppState, ui: &UiState) -> Option<usize> {
    if app.messages.is_empty() {
        return None;
    }
    Some(
        ui.chat_scroll
            .selected_idx
            .unwrap_or(app.messages.len().saturating_sub(1))
            .min(app.messages.len() - 1),
    )
}

fn selected_thread_scope_and_index(app: &AppState, ui: &UiState) -> Option<(MessageScope, usize)> {
    let thread = app.active_thread.as_ref()?;
    let total = 1 + thread.replies.len();
    let sel = ui
        .thread_scroll
        .selected_idx
        .unwrap_or(total.saturating_sub(1))
        .min(total.saturating_sub(1));
    if sel == 0 {
        Some((MessageScope::ThreadRoot, 0))
    } else {
        Some((MessageScope::ThreadReply, sel - 1))
    }
}

fn selected_message_scope_and_index(
    app: &AppState,
    ui: &UiState,
) -> Option<(MessageScope, usize)> {
    match ui.focus() {
        Focus::Thread => selected_thread_scope_and_index(app, ui),
        _ => selected_chat_index(app, ui).map(|i| (MessageScope::Chat, i)),
    }
}

fn maybe_load_older_chats(app: &mut AppState, ui: &UiState) {
    if ui.active_nav_tab != TAB_CHATS {
        return;
    }
    let len = nav_pane::nav_row_count(app, ui);
    if len == 0 {
        return;
    }
    if ui.nav_selected + 1 >= len
        && app.conversations.len() >= 40
        && !app.chats_oldest_reached
        && !app.loading_older_chats
    {
        app.load_older_chats();
    }
}

fn first_selectable_nav_row(rows: &[NavRow]) -> Option<usize> {
    rows.iter().position(NavRow::is_selectable)
}

fn last_selectable_nav_row(rows: &[NavRow]) -> Option<usize> {
    rows.iter().rposition(NavRow::is_selectable)
}

fn step_selectable_nav_row(rows: &[NavRow], start: usize, delta: i32) -> usize {
    let selectable: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(idx, row)| row.is_selectable().then_some(idx))
        .collect();
    if selectable.is_empty() {
        return 0;
    }
    if delta == 0 {
        return start.min(rows.len().saturating_sub(1));
    }
    if delta > 0 {
        let pos = selectable
            .iter()
            .position(|&idx| idx >= start)
            .unwrap_or(selectable.len() - 1);
        let steps = if selectable[pos] == start {
            delta as usize
        } else {
            (delta as usize).saturating_sub(1)
        };
        selectable[pos.saturating_add(steps).min(selectable.len() - 1)]
    } else {
        let pos = selectable.iter().rposition(|&idx| idx <= start).unwrap_or(0);
        let steps = if selectable[pos] == start {
            (-delta) as usize
        } else {
            ((-delta) as usize).saturating_sub(1)
        };
        selectable[pos.saturating_sub(steps)]
    }
}

fn move_nav_selection(app: &mut AppState, ui: &mut UiState, delta: i32) {
    let rows = nav_pane::nav_rows_cached(app, ui);
    if rows.is_empty() {
        return;
    }
    let next = step_selectable_nav_row(&rows, ui.nav_selected, delta);
    ui.nav_selected = next;
    if delta > 0 {
        maybe_load_older_chats(app, ui);
    }
}

fn jump_unread(app: &mut AppState, ui: &mut UiState, forward: bool) {
    let mut unread_map: HashSet<String> = HashSet::new();
    let mut unread_targets: Vec<(String, ConversationTarget)> = Vec::new();
    for conv in &app.conversations {
        if conv.unread_count > 0 && unread_map.insert(conv.conv_key.clone()) {
            unread_targets.push((conv.conv_key.clone(), conv.target.clone()));
        }
    }
    for team in &app.teams {
        for ch in &team.channels {
            let key = ch.conv_key();
            if unread_map.contains(&key) {
                continue;
            }
            if app
                .conversations
                .iter()
                .any(|c| c.conv_key == key && c.unread_count > 0)
            {
                unread_map.insert(key);
                unread_targets.push((ch.conv_key(), ch.target()));
            }
        }
    }
    if unread_targets.is_empty() {
        ui.info("No unread conversations");
        return;
    }
    let idx = if let Some(active) = app.active_conv_key.as_deref() {
        if let Some(pos) = unread_targets.iter().position(|(k, _)| k == active) {
            if forward {
                (pos + 1) % unread_targets.len()
            } else {
                (pos + unread_targets.len() - 1) % unread_targets.len()
            }
        } else if let Some(active_pos) = app.conversations.iter().position(|c| c.conv_key == active)
        {
            if forward {
                app.conversations[active_pos + 1..]
                    .iter()
                    .chain(app.conversations[..active_pos].iter())
                    .find_map(|c| {
                        unread_targets
                            .iter()
                            .position(|(k, _)| k == &c.conv_key)
                    })
                    .unwrap_or(0)
            } else {
                app.conversations[..active_pos]
                    .iter()
                    .rev()
                    .chain(app.conversations[active_pos + 1..].iter().rev())
                    .find_map(|c| {
                        unread_targets
                            .iter()
                            .position(|(k, _)| k == &c.conv_key)
                    })
                    .unwrap_or(unread_targets.len() - 1)
            }
        } else if forward {
            0
        } else {
            unread_targets.len() - 1
        }
    } else if forward {
        0
    } else {
        unread_targets.len() - 1
    };

    let (_, target) = unread_targets[idx].clone();
    app.select_conversation(ui, target);
}

fn step_active_nav_conversation(app: &mut AppState, ui: &mut UiState, delta: i32) {
    let rows = nav_pane::nav_rows_cached(app, ui);
    let entries: Vec<(usize, String)> = rows
        .iter()
        .enumerate()
        .filter_map(|(idx, row)| match row {
            NavRow::Chat { conv_key, .. } | NavRow::TeamChannel { conv_key, .. } => {
                Some((idx, conv_key.clone()))
            }
            _ => None,
        })
        .collect();
    if entries.is_empty() {
        return;
    }
    let cur_pos = app
        .active_conv_key
        .as_deref()
        .and_then(|active| entries.iter().position(|(_, k)| k == active))
        .or_else(|| entries.iter().position(|(idx, _)| *idx == ui.nav_selected));
    let next_pos = match cur_pos {
        Some(pos) if delta > 0 => (pos + 1) % entries.len(),
        Some(pos) => (pos + entries.len() - 1) % entries.len(),
        None if delta > 0 => 0,
        None => entries.len() - 1,
    };
    let (row_idx, conv_key) = entries[next_pos].clone();
    ui.nav_selected = row_idx;
    if let Some(target) = app.target_for_conv_key(&conv_key) {
        app.select_conversation(ui, target);
    }
}

fn active_stream_half_page(ui: &UiState) -> i32 {
    let h = if ui.thread_drawer_open && ui.composer_target_thread {
        ui.thread_scroll.area.height
    } else if matches!(ui.focus(), Focus::Thread) {
        ui.thread_scroll.area.height
    } else {
        ui.chat_scroll.area.height
    };
    ((h / 2).max(5)) as i32
}

fn scroll_active_stream_from_composer(app: &mut AppState, ui: &mut UiState, delta: i32) {
    if ui.thread_drawer_open && ui.composer_target_thread {
        if delta < 0 {
            ui.thread_scroll.scroll_up((-delta) as u16);
        } else if delta > 0 {
            ui.thread_scroll.scroll_down(delta as u16);
        }
    } else {
        scroll_chat_with_autoload(app, ui, delta);
    }
}

fn chat_scroll_is_at_top(ui: &UiState) -> bool {
    let height = ui.chat_scroll.area.height as usize;
    if ui.chat_scroll.total_lines > 0 && height > 0 {
        ui.chat_scroll.offset(height) == 0
    } else {
        ui.chat_scroll.offset == 0
    }
}

fn scroll_chat_with_autoload(app: &mut AppState, ui: &mut UiState, delta: i32) {
    ui.pending_append_anchor = None;
    let was_at_top = chat_scroll_is_at_top(ui);
    if delta < 0 {
        ui.chat_scroll.scroll_up((-delta) as u16);
    } else if delta > 0 {
        ui.chat_scroll.scroll_down(delta as u16);
    }
    let now_at_top = chat_scroll_is_at_top(ui);
    if delta < 0 && (was_at_top || now_at_top) {
        app.maybe_autoload_older_on_scroll_top(ui);
    }
}

fn move_selection(app: &mut AppState, ui: &mut UiState, delta: i32) {
    match ui.focus() {
        Focus::Nav => {
            move_nav_selection(app, ui, delta);
        }
        Focus::Chat => {
            let len = app.messages.len();
            if len == 0 {
                if delta < 0 {
                    app.maybe_autoload_older_on_scroll_top(ui);
                }
                return;
            }
            let cur = ui.chat_scroll.selected_idx.unwrap_or(len - 1) as i32;
            let was_at_first = cur == 0;
            let next = (cur + delta).clamp(0, (len - 1) as i32) as usize;
            ui.chat_scroll.selected_idx = Some(next);
            let vh = ui.chat_scroll.area.height as usize;
            if delta < 0 && next == 0 {
                ui.chat_scroll.scroll_up(2);
                ui.chat_scroll.ensure_visible(next, vh);
                if was_at_first || chat_scroll_is_at_top(ui) {
                    app.maybe_autoload_older_on_scroll_top(ui);
                }
            } else if delta > 0 && next == len - 1 {
                ui.chat_scroll.bottom();
            } else {
                ui.chat_scroll.ensure_visible(next, vh);
            }
        }
        Focus::Thread => {
            let len = app
                .active_thread
                .as_ref()
                .map(|t| 1 + t.replies.len())
                .unwrap_or(0);
            if len == 0 {
                return;
            }
            let cur = ui.thread_scroll.selected_idx.unwrap_or(len - 1) as i32;
            let next = (cur + delta).clamp(0, (len - 1) as i32) as usize;
            ui.thread_scroll.selected_idx = Some(next);
            let vh = ui.thread_scroll.area.height as usize;
            if delta < 0 && next == 0 {
                ui.thread_scroll.scroll_up(2);
                ui.thread_scroll.ensure_visible(next, vh);
            } else if delta > 0 && next == len - 1 {
                ui.thread_scroll.bottom();
            } else {
                ui.thread_scroll.ensure_visible(next, vh);
            }
        }
        Focus::Composer | Focus::Overlay => {}
    }
}

fn scroll_focused_and_autoload(app: &mut AppState, ui: &mut UiState, delta: i32) {
    match ui.focus() {
        Focus::Nav => {
            move_nav_selection(app, ui, delta);
        }
        Focus::Thread => {
            if delta < 0 {
                ui.thread_scroll.scroll_up((-delta) as u16);
            } else if delta > 0 {
                ui.thread_scroll.scroll_down(delta as u16);
            }
        }
        _ => scroll_chat_with_autoload(app, ui, delta),
    }
}

fn handle_mouse_scroll(app: &mut AppState, ui: &mut UiState, delta: i32, column: u16, row: u16) {
    let pos = Position { x: column, y: row };
    if ui.nav_area.contains(pos) {
        move_nav_selection(app, ui, delta);
    } else if ui.thread_drawer_open
        && (ui.thread_scroll.area.contains(pos) || ui.thread_composer_area.contains(pos))
    {
        if delta < 0 {
            ui.thread_scroll.scroll_up((-delta) as u16);
        } else if delta > 0 {
            ui.thread_scroll.scroll_down(delta as u16);
        }
    } else {
        scroll_chat_with_autoload(app, ui, delta);
    }
}

fn handle_activate(app: &mut AppState, ui: &mut UiState) {
    match ui.focus() {
        Focus::Nav => {
            let idx = ui.nav_selected;
            app.select_nav_row(ui, idx);
        }
        Focus::Chat => {
            if let Some(idx) = selected_chat_index(app, ui) {
                if app
                    .active_target
                    .as_ref()
                    .is_some_and(ConversationTarget::is_channel)
                {
                    app.open_thread(ui, idx);
                } else {
                    let _ = dispatch(
                        Action::ReplyTo {
                            scope: MessageScope::Chat,
                            msg_index: idx,
                        },
                        app,
                        ui,
                    );
                }
            }
        }
        Focus::Thread => {
            ui.composer_target_thread = true;
            ui.set_focus(Focus::Composer);
        }
        Focus::Composer | Focus::Overlay => {}
    }
}

fn handle_click(column: u16, row: u16, app: &mut AppState, ui: &mut UiState) -> bool {
    let target = ui.hitmap.hit(row, column);

    if !ui.overlays.is_empty() {
        match target {
            Some(HitTarget::OverlayItem { index }) => {
                let is_ctx = ui
                    .overlays
                    .last()
                    .is_some_and(|o| o.kind() == OverlayKind::ContextMenu);
                dispatch(Action::OverlaySelect(index), app, ui);
                if is_ctx {
                    return dispatch(Action::Activate, app, ui);
                }
                return false;
            }
            Some(HitTarget::Button(ButtonId::ConfirmOk)) => {
                return dispatch(Action::Activate, app, ui);
            }
            Some(HitTarget::Button(ButtonId::ConfirmCancel)) => {
                ui.close_top_overlay();
                return false;
            }
            Some(HitTarget::Button(ButtonId::HtmlToggleRaw)) => {
                if let Some(Overlay::HtmlPreview(preview)) = ui.overlays.last_mut() {
                    preview.toggle_raw_mode();
                }
                return false;
            }
            Some(HitTarget::Button(ButtonId::HtmlOpenBrowser)) => {
                return dispatch(Action::ServeHtmlPreview, app, ui);
            }
            Some(HitTarget::OverlayBody) => {
                if let Some(Overlay::HtmlPreview(preview)) = ui.overlays.last_mut() {
                    let outcome = preview.handle_screen_click(column, row);
                    if let OverlayOutcome::Dispatch(next) = outcome {
                        return dispatch(next, app, ui);
                    }
                }
                return false;
            }
            // Clicking outside the modal (either empty background or an underlying pane target) closes the top overlay.
            _ => {
                ui.close_top_overlay();
                return false;
            }
        }
    }

    if let Some(hit) = target {
        let now = Instant::now();
        let is_double_click = matches!(hit, HitTarget::Message { .. })
            && ui.last_click.is_some_and(|(t, _, _, prev_hit)| {
                prev_hit == hit && now.duration_since(t) <= Duration::from_millis(400)
            });
        if is_double_click {
            ui.last_click = None;
            if let HitTarget::Message { scope, msg_index } = hit {
                return dispatch(Action::ReplyTo { scope, msg_index }, app, ui);
            }
        }
        ui.last_click = Some((now, column, row, hit));

        if hit == HitTarget::ThreadComposer {
            if !ui.composer_target_thread {
                app.stash_and_reset_draft(ui);
                ui.composer_target_thread = true;
                app.restore_draft(ui);
            }
            return dispatch(Action::OpenComposer, app, ui);
        }
        if hit == HitTarget::Composer {
            if ui.thread_drawer_open && ui.term_width >= 120 && ui.composer_target_thread {
                app.stash_and_reset_draft(ui);
                ui.composer_target_thread = false;
                app.restore_draft(ui);
            }
            return dispatch(Action::OpenComposer, app, ui);
        }

        if let HitTarget::Attachment {
            scope,
            msg_index,
            attach_index,
        } = hit
        {
            click_attachment(scope, msg_index, attach_index, app, ui);
            return false;
        }

        let act = action_for_target(hit);
        return dispatch(act, app, ui);
    }

    ui.last_click = None;
    let pos = Position { x: column, y: row };
    if ui.thread_drawer_open && ui.thread_scroll.area.contains(pos) {
        ui.set_focus(Focus::Thread);
    } else if ui.chat_scroll.area.contains(pos) {
        ui.set_focus(Focus::Chat);
    } else if ui.nav_area.contains(pos) {
        ui.set_focus(Focus::Nav);
    }
    false
}

fn click_attachment(
    scope: MessageScope,
    msg_index: usize,
    attach_index: usize,
    app: &mut AppState,
    ui: &mut UiState,
) {
    let _ = dispatch(Action::SelectMessage { scope, msg_index }, app, ui);
    ui.attach_index = attach_index;

    let clicked_att = app
        .message_at(scope, msg_index)
        .and_then(|msg| msg.effective_attachments().get(attach_index).cloned());

    if let Some(att) = clicked_att {
        if HtmlPreviewOverlay::is_html_attachment(&att) {
            if att.content_url.is_some() {
                app.fetch_html_attachment(ui, att);
                return;
            }
            if let Some(inline_html) = att.content_json.as_deref().filter(|s| !s.trim().is_empty()) {
                ui.close_overlays_of(OverlayKind::HtmlPreview);
                ui.push_overlay(Overlay::html_preview(att.name, inline_html.to_string()));
                return;
            }
        }
        if HtmlPreviewOverlay::is_card_attachment(&att) {
            if let Some((title, html)) = adaptive_card_to_html(&att) {
                ui.close_overlays_of(OverlayKind::HtmlPreview);
                ui.push_overlay(Overlay::html_preview(Some(title), html));
                return;
            }
        }
    }

    let preview_opt = app.message_at(scope, msg_index).and_then(|msg| {
        let eff = msg.effective_attachments();
        let is_img = eff.get(attach_index).is_some_and(|att| {
            crate::img_preview::is_image_mime_or_name(&att.content_type, att.name.as_deref())
        });
        if is_img {
            ImagePreviewOverlay::from_teams_message(msg, Some(attach_index))
        } else {
            None
        }
    });

    if let Some(preview) = preview_opt {
        push_image_preview(preview, app, ui);
    } else {
        app.download_attachment(ui, scope, msg_index, attach_index);
    }
}

fn action_for_target(target: HitTarget) -> Action {
    match target {
        HitTarget::NavTab(tab) => Action::SelectNavTab(tab),
        HitTarget::NavRow(row) => Action::SelectNavRow(row),
        HitTarget::Message { scope, msg_index } => Action::SelectMessage { scope, msg_index },
        HitTarget::Attachment {
            scope,
            msg_index,
            attach_index,
        } => Action::DownloadAttachment {
            scope,
            msg_index,
            attach_index,
        },
        HitTarget::Reaction {
            scope,
            msg_index,
            reaction_index,
        } => Action::ToggleReactionByIndex {
            scope,
            msg_index,
            reaction_index,
        },
        HitTarget::OpenThread { msg_index } => Action::OpenThread { msg_index },
        HitTarget::LoadOlderMessages => Action::LoadOlder,
        HitTarget::Button(btn) => match btn {
            ButtonId::Search => Action::OpenOverlay(OverlayKind::Search),
            ButtonId::SendFile => Action::OpenOverlay(OverlayKind::PathInput),
            ButtonId::Attachments => Action::OpenOverlay(OverlayKind::Attachments),
            ButtonId::RoomInfo => Action::OpenOverlay(OverlayKind::RoomInfo),
            ButtonId::Refresh => Action::RefreshAll,
            ButtonId::Settings => Action::OpenOverlay(OverlayKind::Settings),
            ButtonId::NewChat => Action::OpenOverlay(OverlayKind::NewChat),
            ButtonId::Presence => Action::OpenOverlay(OverlayKind::Presence),
            ButtonId::CloseThread => Action::CloseThreadDrawer,
            ButtonId::ComposerTargetToggle => Action::ToggleComposerTarget,
            ButtonId::SubjectToggle => Action::ToggleSubject,
            ButtonId::NarrowToggle => Action::ToggleNarrowPane,
            ButtonId::NarrowList => Action::SelectNarrowPane(NarrowPane::List),
            ButtonId::NarrowChat => Action::SelectNarrowPane(NarrowPane::Chat),
            ButtonId::NarrowThread => Action::SelectNarrowPane(NarrowPane::Thread),
            ButtonId::ImagePreview => Action::OpenImagePreview,
            ButtonId::HtmlPreview => Action::OpenHtmlPreview,
            ButtonId::HtmlToggleRaw => Action::Input('m'),
            ButtonId::HtmlOpenBrowser => Action::ServeHtmlPreview,
            ButtonId::CommandPalette => Action::OpenOverlay(OverlayKind::CommandPalette),
            ButtonId::DensityToggle => Action::ToggleDensity,
            ButtonId::Help => Action::OpenOverlay(OverlayKind::Help),
            ButtonId::FilterToggle => Action::ToggleUnreadFilter,
            ButtonId::ConfirmOk => Action::Activate,
            ButtonId::ConfirmCancel => Action::CloseOverlay,
        },
        HitTarget::InlineAction {
            scope,
            msg_index,
            action,
        } => match action {
            super::hitmap::InlineActionKind::Reply => Action::ReplyTo { scope, msg_index },
            super::hitmap::InlineActionKind::Thread => Action::OpenThread { msg_index },
            super::hitmap::InlineActionKind::React => Action::OpenReactions { scope, msg_index },
            super::hitmap::InlineActionKind::HtmlPreview => {
                Action::OpenHtmlPreviewForMessage { scope, msg_index }
            }
            super::hitmap::InlineActionKind::More => Action::SelectMessage { scope, msg_index },
        },
        HitTarget::SubjectInput => Action::FocusSubject,
        HitTarget::Composer => Action::OpenComposer,
        HitTarget::ThreadComposer => Action::OpenComposer,
        HitTarget::OverlayBody => Action::Noop,
        HitTarget::OverlayItem { index } => Action::OverlaySelect(index),
        HitTarget::MentionCandidate(index) => Action::SelectMention(index),
    }
}

fn open_context_menu_at(column: u16, row: u16, app: &mut AppState, ui: &mut UiState) {
    let ctx = match ui.hitmap.hit(row, column) {
        Some(HitTarget::NavRow(idx)) => ContextTarget::NavRow(idx),
        Some(HitTarget::Message { scope, msg_index })
        | Some(HitTarget::Attachment {
            scope, msg_index, ..
        })
        | Some(HitTarget::Reaction {
            scope, msg_index, ..
        }) => ContextTarget::Message { scope, msg_index },
        Some(HitTarget::OpenThread { msg_index }) => ContextTarget::Message {
            scope: MessageScope::Chat,
            msg_index,
        },
        Some(HitTarget::Composer)
        | Some(HitTarget::ThreadComposer)
        | Some(HitTarget::SubjectInput) => ContextTarget::Composer,
        _ => ContextTarget::Global,
    };
    ui.close_overlays_of(OverlayKind::ContextMenu);
    let menu = ContextMenu::build(ctx, (column, row), app, ui);
    ui.push_overlay(Overlay::context_menu(menu));
}

fn copy_to_clipboard(text: &str, ui: &mut UiState) {
    let mut last_err: Option<String> = None;
    #[allow(unused_mut)]
    let mut native_ok = match with_clipboard(|cb| cb.set_text(text.to_string())) {
        Ok(()) => true,
        Err(err) => {
            last_err = Some(err.to_string());
            false
        }
    };

    #[cfg(target_os = "linux")]
    if !native_ok {
        native_ok = linux_cli_copy_clipboard(text);
    }

    let is_ssh = has_nonempty_env("SSH_TTY") || has_nonempty_env("SSH_CONNECTION");
    let osc_ok = if !native_ok || is_ssh {
        emit_osc52(text)
    } else {
        false
    };

    if native_ok {
        ui.info("Copied to clipboard");
    } else if osc_ok {
        ui.info("Copied to clipboard (OSC 52)");
    } else if let Some(err) = last_err {
        ui.info(format!("Clipboard copy failed: {err}"));
    } else {
        ui.info("Clipboard copy failed: no clipboard backend available");
    }
}

fn paste_text_into_ui(text: String, app: &mut AppState, ui: &mut UiState) {
    if !ui.overlays.is_empty() {
        dispatch(Action::Paste(text), app, ui);
        return;
    }
    if !matches!(ui.focus(), Focus::Composer) {
        ui.set_focus(Focus::Composer);
    }
    ui.composer.insert_str(&text);
    ui.composer.reference_mentions(&app.members);
}

pub(crate) fn save_pasted_png_in_dir(
    dir: &Path,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    unix_ms: u128,
) -> Result<(PathBuf, Vec<u8>), String> {
    let png_bytes = composer::encode_rgba_to_png(width, height, rgba)?;
    let file_name = format!("teams-pasted-{unix_ms}.png");
    let path = dir.join(&file_name);
    std::fs::write(&path, &png_bytes)
        .map_err(|err| format!("Failed to write {}: {err}", path.display()))?;
    Ok((path, png_bytes))
}

fn current_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn paste_clipboard(app: &mut AppState, ui: &mut UiState) {
    let in_overlay = !ui.overlays.is_empty();
    let mut arboard_opened = false;

    // In the composer, check clipboard image data first so pasted screenshots/images always attach cleanly.
    if !in_overlay {
        let img_res = with_clipboard(|cb| {
            arboard_opened = true;
            cb.get_image()
        });
        if let Ok(img) = img_res {
            let width = img.width as u32;
            let height = img.height as u32;
            let rgba = img.bytes.into_owned();
            let unix_ms = current_unix_ms();
            let temp_dir = std::env::temp_dir();
            if tokio::runtime::Handle::try_current().is_ok() {
                let task_label = format!("Paste image ({width}x{height})");
                ui.tasks.begin(task_label.clone());
                ui.set_focus(Focus::Composer);
                ui.info(format!("Encoding clipboard image ({width}x{height})…"));
                let tx = app.ui_tx.clone();
                tokio::task::spawn_blocking(move || {
                    match save_pasted_png_in_dir(&temp_dir, width, height, rgba, unix_ms) {
                        Ok((saved_path, png_bytes)) => {
                            let file_name = saved_path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("teams-pasted.png")
                                .to_string();
                            let preview = crate::img_preview::decode_and_render_halfblocks(
                                &png_bytes,
                                24,
                                5,
                                (20, 22, 26),
                            )
                            .ok();
                            let toast_msg = format!(
                                "Attached clipboard image: {} ({width}x{height})",
                                saved_path.display()
                            );
                            let _ = tx.send(super::app::UiMsg::ClipboardImageEncoded {
                                width,
                                height,
                                file_name,
                                png_bytes,
                                preview,
                            });
                            let _ = tx.send(super::app::UiMsg::TaskDone {
                                label: task_label,
                                error: None,
                                toast: Some((toast_msg, super::toast::ToastKind::Info)),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(super::app::UiMsg::TaskDone {
                                label: task_label,
                                error: Some(err.clone()),
                                toast: Some((
                                    format!("Image encoding failed: {err}"),
                                    super::toast::ToastKind::Error,
                                )),
                            });
                        }
                    }
                });
            } else {
                match save_pasted_png_in_dir(&temp_dir, width, height, rgba, unix_ms) {
                    Ok((saved_path, png_bytes)) => {
                        let file_name = saved_path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("teams-pasted.png")
                            .to_string();
                        ui.composer.stage_inline_image(
                            png_bytes,
                            "image/png",
                            Some(file_name),
                        );
                        ui.set_focus(Focus::Composer);
                        ui.info(format!(
                            "Attached clipboard image: {} ({width}x{height})",
                            saved_path.display()
                        ));
                    }
                    Err(err) => {
                        ui.error(format!("Image encoding failed: {err}"));
                    }
                }
            }
            return;
        }
    }

    if let Ok(text) = with_clipboard(|cb| {
        arboard_opened = true;
        cb.get_text()
    }) {
        if !text.is_empty() {
            paste_text_into_ui(text, app, ui);
            return;
        }
    }

    #[cfg(target_os = "linux")]
    if has_nonempty_env("WAYLAND_DISPLAY") {
        if let Some(text) = linux_wayland_paste_text() {
            if !text.is_empty() {
                paste_text_into_ui(text, app, ui);
                return;
            }
        }
    }

    if !arboard_opened {
        ui.info("Unable to open clipboard");
    }
}

thread_local! {
    static CLIPBOARD: std::cell::RefCell<Option<arboard::Clipboard>> =
        const { std::cell::RefCell::new(None) };
}

fn has_nonempty_env(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

fn with_clipboard<T>(
    f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
) -> Result<T, arboard::Error> {
    #[cfg(target_os = "linux")]
    if !has_nonempty_env("DISPLAY") && !has_nonempty_env("WAYLAND_DISPLAY") {
        return Err(arboard::Error::ClipboardNotSupported);
    }

    CLIPBOARD.with(|slot| {
        let mut guard = slot.borrow_mut();
        if guard.is_none() {
            *guard = Some(arboard::Clipboard::new()?);
        }
        let cb = guard
            .as_mut()
            .ok_or(arboard::Error::ClipboardNotSupported)?;
        f(cb)
    })
}

#[cfg(target_os = "linux")]
fn pipe_to_clipboard_cmd(program: &str, args: &[&str], text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.wait();
            return false;
        }
    }
    child.wait().is_ok_and(|s| s.success())
}

#[cfg(target_os = "linux")]
fn linux_cli_copy_clipboard(text: &str) -> bool {
    if has_nonempty_env("WAYLAND_DISPLAY") && pipe_to_clipboard_cmd("wl-copy", &[], text) {
        return true;
    }
    if has_nonempty_env("DISPLAY") {
        if pipe_to_clipboard_cmd("xclip", &["-selection", "clipboard"], text) {
            return true;
        }
        if pipe_to_clipboard_cmd("xsel", &["--clipboard", "--input"], text) {
            return true;
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn linux_wayland_paste_text() -> Option<String> {
    use std::process::{Command, Stdio};

    let output = Command::new("wl-paste")
        .arg("--no-newline")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

const OSC52_MAX_RAW_BYTES: usize = 74_994;

fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(crate) fn format_osc52_sequence(text: &str, in_tmux: bool) -> String {
    let clipped = truncate_utf8_bytes(text, OSC52_MAX_RAW_BYTES);
    let encoded = encode_base64(clipped.as_bytes());
    if in_tmux {
        format!("\x1bPtmux;\x1b\x1b]52;c;{encoded}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{encoded}\x07")
    }
}

fn emit_osc52(text: &str) -> bool {
    use std::io::Write;
    let seq = format_osc52_sequence(text, has_nonempty_env("TMUX"));
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(seq.as_bytes()).is_ok() && stdout.flush().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_base64_matches_rfc4648_vectors() {
        assert_eq!(encode_base64(b""), "");
        assert_eq!(encode_base64(b"f"), "Zg==");
        assert_eq!(encode_base64(b"fo"), "Zm8=");
        assert_eq!(encode_base64(b"foo"), "Zm9v");
        assert_eq!(encode_base64(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn format_osc52_sequence_plain_and_tmux() {
        assert_eq!(
            format_osc52_sequence("hello", false),
            "\x1b]52;c;aGVsbG8=\x07"
        );
        assert_eq!(
            format_osc52_sequence("hello", true),
            "\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\"
        );
    }

    #[test]
    fn format_osc52_sequence_caps_oversized_utf8_cleanly() {
        // 3-byte Korean char repeated to exceed 74,994 bytes.
        let big = "가".repeat(30_000);
        let seq = format_osc52_sequence(&big, false);
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
        // 74,994 raw bytes -> 99,992 base64 bytes + 8 bytes framing = 100,000 bytes.
        assert!(seq.len() <= 100_000);
    }

    fn test_app_and_ui() -> (AppState, UiState) {
        let cfg = crate::config::AppConfig::default();
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl =
            crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui)
    }

    #[tokio::test]
    async fn composer_scroll_paths_keep_composer_focus() {
        let (mut app, mut ui) = test_app_and_ui();
        ui.set_focus(Focus::Composer);
        ui.chat_scroll.area = ratatui::layout::Rect::new(0, 0, 80, 20);
        ui.chat_scroll.total_lines = 100;

        // Path 1: PageUp / PageDown
        dispatch(Action::ScrollPageUp, &mut app, &mut ui);
        assert_eq!(ui.focus(), Focus::Composer);
        assert_eq!(ui.chat_scroll.scroll_up, 10);
        assert!(!ui.chat_scroll.follow_tail);

        dispatch(Action::ScrollPageDown, &mut app, &mut ui);
        assert_eq!(ui.focus(), Focus::Composer);
        assert_eq!(ui.chat_scroll.scroll_up, 0);
        assert!(ui.chat_scroll.follow_tail);

        // Path 2: Ctrl+Up / Ctrl+Down even with multiline text
        ui.composer.load_text("line1\nline2");
        dispatch(Action::ScrollUp, &mut app, &mut ui);
        assert_eq!(ui.focus(), Focus::Composer);
        assert_eq!(ui.chat_scroll.scroll_up, 3);

        // Path 3: Up/Down when composer has text moves cursor, NOT stream scroll
        let cursor_before = ui.composer.cursor();
        dispatch(Action::CursorUp, &mut app, &mut ui);
        assert!(ui.composer.cursor() < cursor_before);
        assert_eq!(ui.chat_scroll.scroll_up, 3);

        // Path 3b: Up/Down when composer has NO text scrolls stream, keeping Focus::Composer
        ui.composer.load_text("");
        dispatch(Action::CursorUp, &mut app, &mut ui);
        assert_eq!(ui.focus(), Focus::Composer);
        assert_eq!(ui.chat_scroll.scroll_up, 6);

        dispatch(Action::CursorDown, &mut app, &mut ui);
        assert_eq!(ui.focus(), Focus::Composer);
        assert_eq!(ui.chat_scroll.scroll_up, 3);

        // Overscrolling past the top clamps to max_off (80), so a single wheel-down immediately moves.
        for _ in 0..40 {
            dispatch(
                Action::MouseScroll {
                    delta: -3,
                    column: 10,
                    row: 10,
                },
                &mut app,
                &mut ui,
            );
        }
        assert_eq!(ui.chat_scroll.scroll_up, 80);
        dispatch(
            Action::MouseScroll {
                delta: 3,
                column: 10,
                row: 10,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(ui.chat_scroll.scroll_up, 77);
        assert_eq!(ui.chat_scroll.offset(20), 3);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn quit_confirm_opens_modal_when_draft_nonempty_and_overlay_body_blocks_dismiss() {
        let (mut app, mut ui) = test_app_and_ui();
        ui.composer.load_text("unsent draft");
        let should_quit = dispatch(Action::QuitConfirm, &mut app, &mut ui);
        assert!(!should_quit);
        assert_eq!(ui.overlays.len(), 1);
        assert_eq!(ui.overlays[0].kind(), OverlayKind::Confirm);

        ui.hitmap.clear();
        ui.hitmap.add(
            ratatui::layout::Rect::new(10, 5, 40, 8),
            HitTarget::OverlayBody,
        );
        // Clicking inside OverlayBody keeps modal open
        assert!(!dispatch(
            Action::Click { column: 15, row: 7 },
            &mut app,
            &mut ui
        ));
        assert_eq!(ui.overlays.len(), 1);

        // Clicking outside closes modal
        assert!(!dispatch(
            Action::Click { column: 0, row: 0 },
            &mut app,
            &mut ui
        ));
        assert!(ui.overlays.is_empty());

        // Empty composer exits immediately on Ctrl+Q
        ui.composer.load_text("");
        assert!(dispatch(Action::QuitConfirm, &mut app, &mut ui));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn narrow_bar_buttons_and_thread_composer_click_targets() {
        let (mut app, mut ui) = test_app_and_ui();
        ui.term_width = 140;
        ui.thread_drawer_open = true;
        ui.composer_target_thread = true;
        app.active_thread = Some(crate::models::ThreadView {
            conv_key: "chan:t1:c1".to_string(),
            root: crate::models::MessageItem {
                msg_key: "chan:t1:c1|r1".to_string(),
                conv_key: "chan:t1:c1".to_string(),
                msg_id: "r1".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: None,
                author_name: "Alice".to_string(),
                is_me: false,
                created_at: "2026-11-18T00:00:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: String::new(),
                body_plain: "hi".to_string(),
                attachments: vec![],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            },
            replies: vec![],
        });

        ui.hitmap.clear();
        ui.hitmap.add(
            ratatui::layout::Rect::new(80, 30, 40, 5),
            HitTarget::ThreadComposer,
        );
        dispatch(Action::Click { column: 90, row: 32 }, &mut app, &mut ui);
        assert!(ui.composer_target_thread);
        assert_eq!(ui.focus(), Focus::Composer);

        dispatch(Action::SelectNarrowPane(NarrowPane::List), &mut app, &mut ui);
        assert_eq!(ui.narrow_pane, NarrowPane::List);
        assert_eq!(ui.focus(), Focus::Nav);

        dispatch(Action::SelectNarrowPane(NarrowPane::Chat), &mut app, &mut ui);
        assert_eq!(ui.narrow_pane, NarrowPane::Chat);
        assert_eq!(ui.focus(), Focus::Composer);

        dispatch(Action::SelectNarrowPane(NarrowPane::Thread), &mut app, &mut ui);
        assert_eq!(ui.narrow_pane, NarrowPane::Thread);
        assert_eq!(ui.focus(), Focus::Thread);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn image_preview_cycle_protocol_dispatch_and_ui_draw_integration() {
        use crate::img_preview::{RenderedTerminalImage, TerminalImageProtocol};
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = test_app_and_ui();
        ui.term_width = 100;
        ui.term_height = 30;
        ui.graphics_caps.protocol = TerminalImageProtocol::Halfblock;

        let msg = crate::models::MessageItem {
            msg_key: "chat:c1|m-img".to_string(),
            conv_key: "chat:c1".to_string(),
            msg_id: "m-img".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: "2026-11-18T00:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: vec![crate::models::AttachmentInfo {
                id: "att-1".to_string(),
                content_type: "image/png".to_string(),
                content_url: None,
                name: Some("pic.png".to_string()),
                content_json: None,
                thumbnail_url: None,
            }],
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        };
        let overlay = ImagePreviewOverlay::from_teams_message(&msg, Some(0)).unwrap();
        let (cols, rows) = overlay.target_dimensions(ui.term_width, ui.term_height);
        ui.push_overlay(Overlay::ImagePreview(overlay));

        // Pressing 'p' cycles protocol from Halfblock -> next_manual() and queues LoadPreviewImage.
        let expected_next = TerminalImageProtocol::Halfblock.next_manual();
        assert!(!dispatch(Action::Input('p'), &mut app, &mut ui));
        assert_eq!(ui.graphics_caps.protocol, expected_next);

        // Cache a pixel payload for the new protocol and verify ui::draw populates pending_pixel_graphic.
        let key = AppState::preview_image_cache_key("m-img", "att-1", cols, rows, expected_next);
        app.insert_image_cache(
            key.clone(),
            RenderedTerminalImage {
                width_px: 100,
                height_px: 80,
                cols: 20,
                rows: 8,
                format_label: "PNG".to_string(),
                lines: Vec::new(),
                protocol: expected_next,
                pixel_payload: Some("PIXEL_PAYLOAD".to_string()),
                scroll_backing: None,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                let _ = crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();
        assert_eq!(
            ui.pending_pixel_graphic.as_ref().map(|p| (&p.cache_key, p.protocol)),
            Some((&key, expected_next))
        );

        // Closing overlay resets pending_pixel_graphic to None on the next ui::draw.
        assert!(!dispatch(Action::CloseOverlay, &mut app, &mut ui));
        terminal
            .draw(|frame| {
                let _ = crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();
        assert!(ui.pending_pixel_graphic.is_none());

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn click_attachment_opens_preview_for_hosted_image_and_downloads_for_pdf() {
        let (mut app, mut ui) = test_app_and_ui();
        app.messages = vec![
            crate::models::MessageItem {
                msg_key: "chat:c1|m1".to_string(),
                conv_key: "chat:c1".to_string(),
                msg_id: "m1".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: None,
                author_name: "Alice".to_string(),
                is_me: false,
                created_at: "2026-11-18T00:00:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: "<p>Look:</p><img src=\"https://graph.microsoft.com/v1.0/chats/c1/messages/m1/hostedContents/1/$value\">".to_string(),
                body_plain: "Look:".to_string(),
                attachments: Vec::new(),
                mentions: Vec::new(),
                reactions: Vec::new(),
                reply_count: 0,
                web_url: None,
            },
            crate::models::MessageItem {
                msg_key: "chat:c1|m2".to_string(),
                conv_key: "chat:c1".to_string(),
                msg_id: "m2".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: None,
                author_name: "Bob".to_string(),
                is_me: false,
                created_at: "2026-11-18T00:01:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: String::new(),
                body_plain: String::new(),
                attachments: vec![crate::models::AttachmentInfo {
                    id: "att-pdf".to_string(),
                    content_type: "application/pdf".to_string(),
                    content_url: Some("https://contoso.sharepoint.com/report.pdf".to_string()),
                    name: Some("report.pdf".to_string()),
                    content_json: None,
                    thumbnail_url: None,
                }],
                mentions: Vec::new(),
                reactions: Vec::new(),
                reply_count: 0,
                web_url: None,
            },
        ];

        ui.hitmap.clear();
        ui.hitmap.add_line(
            2,
            5,
            20,
            HitTarget::Attachment {
                scope: MessageScope::Chat,
                msg_index: 0,
                attach_index: 0,
            },
        );
        ui.hitmap.add_line(
            2,
            8,
            20,
            HitTarget::Attachment {
                scope: MessageScope::Chat,
                msg_index: 1,
                attach_index: 0,
            },
        );

        // Clicking inline hosted image attachment opens Overlay::ImagePreview focused on hosted:1.
        assert!(!dispatch(Action::Click { column: 4, row: 5 }, &mut app, &mut ui));
        assert_eq!(ui.chat_scroll.selected_idx, Some(0));
        assert_eq!(ui.attach_index, 0);
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::ImagePreview(p))
                if p.items.len() == 1 && p.current().is_some_and(|t| t.att_id() == "hosted:1")
        ));

        // Close preview, then click a PDF attachment -> triggers download instead of ImagePreview.
        ui.overlays.clear();
        assert!(!dispatch(Action::Click { column: 4, row: 8 }, &mut app, &mut ui));
        assert_eq!(ui.chat_scroll.selected_idx, Some(1));
        assert!(ui.overlays.is_empty());
        assert!(ui
            .tasks
            .first_running()
            .is_some_and(|t| t.label.contains("Download: report.pdf")));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[test]
    fn alt_v_in_composer_maps_to_open_image_preview() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let keymap = crate::ui::keymap::Keymap::default_teams();
        let action = crate::ui::event::key_to_action(
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT),
            Focus::Composer,
            &keymap,
            true,
        );
        assert_eq!(action, Some(Action::OpenImagePreview));
    }

    #[test]
    fn save_pasted_png_in_dir_writes_valid_png_file() {
        let dir = tempfile::tempdir().unwrap();
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255];
        let (path, png_bytes) =
            save_pasted_png_in_dir(dir.path(), 2, 2, rgba, 1_700_000_000_123).unwrap();
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("teams-pasted-1700000000123.png")
        );
        assert!(path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), png_bytes);
        let decoded = image::load_from_memory(&png_bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 2));
    }

    #[tokio::test]
    async fn toggle_show_hrefs_and_open_links_dispatch() {
        let (mut app, mut ui) = test_app_and_ui();
        assert!(!ui.show_hrefs);
        assert!(!dispatch(Action::ToggleShowHrefs, &mut app, &mut ui));
        assert!(ui.show_hrefs);
        assert!(!dispatch(Action::ToggleShowHrefs, &mut app, &mut ui));
        assert!(!ui.show_hrefs);

        app.messages.push(crate::models::MessageItem {
            msg_key: "chat:c1|m-links".to_string(),
            conv_key: "chat:c1".to_string(),
            msg_id: "m-links".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: "2026-11-18T00:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: r#"<p>Check <a href="https://example.com/pr/42">PR #42</a></p>"#.to_string(),
            body_plain: "Check PR #42".to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        });
        ui.set_focus(Focus::Chat);
        ui.chat_scroll.selected_idx = Some(0);

        assert!(!dispatch(Action::OpenLinks, &mut app, &mut ui));
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::Links(l)) if l.links.len() == 1 && l.links[0].url == "https://example.com/pr/42"
        ));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn wired_buttons_register_in_hitmap_and_dispatch_on_click() {
        use ratatui::{backend::TestBackend, Terminal};

        let (mut app, mut ui) = test_app_and_ui();
        let ch_target = ConversationTarget::Channel {
            team_id: "t1".into(),
            channel_id: "ch1".into(),
        };
        app.conversations.push(crate::models::ConversationSummary {
            conv_key: "channel:t1:ch1".into(),
            target: ch_target.clone(),
            kind: crate::models::ConversationKind::Channel,
            team_id: Some("t1".into()),
            team_name: Some("Engineering".into()),
            title: "General".into(),
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
        app.active_conv_key = Some("channel:t1:ch1".into());
        app.active_target = Some(ch_target);
        app.messages.push(crate::models::MessageItem {
            msg_key: "channel:t1:ch1|m1".into(),
            conv_key: "channel:t1:ch1".into(),
            msg_id: "m1".into(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Alice".into(),
            is_me: false,
            created_at: "2026-11-18T00:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: String::new(),
            body_plain: "Here is a screenshot".into(),
            attachments: vec![crate::models::AttachmentInfo {
                id: "att-1".into(),
                content_type: "image/png".into(),
                content_url: None,
                name: Some("shot.png".into()),
                content_json: None,
                thumbnail_url: None,
            }],
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        });
        ui.set_focus(Focus::Chat);

        // Draw Wide layout (140x40) and verify SendFile, Settings, Presence, SubjectToggle, ImagePreview
        let mut wide_term = Terminal::new(TestBackend::new(140, 40)).unwrap();
        wide_term
            .draw(|frame| {
                let _ = crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        let click_btn = |btn: ButtonId, app: &mut AppState, ui: &mut UiState| {
            let rect = ui
                .hitmap
                .region_of(HitTarget::Button(btn))
                .unwrap_or_else(|| panic!("expected ButtonId::{btn:?} in hitmap"));
            dispatch(
                Action::Click {
                    column: rect.x,
                    row: rect.y,
                },
                app,
                ui,
            );
        };

        // 1. SendFile -> opens PathInput overlay
        click_btn(ButtonId::SendFile, &mut app, &mut ui);
        assert_eq!(
            ui.overlays.last().map(Overlay::kind),
            Some(OverlayKind::PathInput)
        );
        ui.close_top_overlay();

        // 2. Settings -> opens Settings overlay
        click_btn(ButtonId::Settings, &mut app, &mut ui);
        assert_eq!(
            ui.overlays.last().map(Overlay::kind),
            Some(OverlayKind::Settings)
        );
        ui.close_top_overlay();

        // 3. Presence -> opens Presence overlay
        click_btn(ButtonId::Presence, &mut app, &mut ui);
        assert_eq!(
            ui.overlays.last().map(Overlay::kind),
            Some(OverlayKind::Presence)
        );
        ui.close_top_overlay();

        // 4. SubjectToggle -> toggles ui.show_subject on channel root post
        assert!(!ui.show_subject);
        click_btn(ButtonId::SubjectToggle, &mut app, &mut ui);
        assert!(ui.show_subject);

        // 5. ImagePreview -> opens ImagePreview overlay
        click_btn(ButtonId::ImagePreview, &mut app, &mut ui);
        assert_eq!(
            ui.overlays.last().map(Overlay::kind),
            Some(OverlayKind::ImagePreview)
        );
        ui.close_top_overlay();

        // 6. Narrow layout (70x24) -> NarrowToggle pill registered and toggles narrow_pane
        ui.narrow_pane = NarrowPane::List;
        let mut narrow_term = Terminal::new(TestBackend::new(70, 24)).unwrap();
        narrow_term
            .draw(|frame| {
                let _ = crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();
        click_btn(ButtonId::NarrowToggle, &mut app, &mut ui);
        assert_eq!(ui.narrow_pane, NarrowPane::Chat);

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn html_preview_entry_points_onclick_raw_toggle_and_native_axum_serving() {
        use ratatui::{backend::TestBackend, Terminal};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let cfg = crate::config::AppConfig::default();
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl =
            crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        app.active_conv_key = Some("chat:c-html".into());
        app.active_target = Some(ConversationTarget::Chat {
            chat_id: "c-html".into(),
        });
        app.messages.push(crate::models::MessageItem {
            msg_key: "chat:c-html|m1".into(),
            conv_key: "chat:c-html".into(),
            msg_id: "m1".into(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: Some("Release Report".into()),
            author_id: None,
            author_name: "Alice".into(),
            is_me: false,
            created_at: "2026-11-20T00:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: r#"<div><p id="status">Pending</p><button onclick="document.getElementById('status').textContent='Approved';">Approve</button></div>"#.into(),
            body_plain: "Pending Approve".into(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        });
        ui.set_focus(Focus::Chat);
        ui.chat_scroll.selected_idx = Some(0);

        // Draw chat and verify both header `[H: HTML]` pill and inline `[H: HTML]` pill exist
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal
            .draw(|frame| {
                let _ = crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();
        let header_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::HtmlPreview))
            .expect("expected header [H: HTML] ButtonId::HtmlPreview");
        let inline_chip = ui
            .hitmap
            .region_of(HitTarget::InlineAction {
                scope: MessageScope::Chat,
                msg_index: 0,
                action: crate::ui::hitmap::InlineActionKind::HtmlPreview,
            })
            .expect("expected inline [H: HTML] chip on selected message");
        assert!(inline_chip.width > 0);

        // Click header `[H: HTML]` pill -> opens Overlay::HtmlPreview with QuickJS active
        dispatch(
            Action::Click {
                column: header_btn.x,
                row: header_btn.y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(
            ui.overlays.last().map(Overlay::kind),
            Some(OverlayKind::HtmlPreview)
        );

        // Draw overlay so hitmap and last_content_area are populated
        terminal
            .draw(|frame| {
                let _ = crate::ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        // Click the <button onclick> using screen coordinates -> Action::Click -> click_at
        let (btn_x, btn_y) = match ui.overlays.last_mut().unwrap() {
            Overlay::HtmlPreview(preview) => {
                assert!(preview.session.js_active);
                let theme = HtmlPreviewOverlay::html_theme(&ui.theme);
                let ctrl = preview
                    .session
                    .render(preview.last_content_area.width, &theme, false)
                    .controls[0]
                    .clone();
                (
                    preview.last_content_area.x + ctrl.col_start + 1,
                    preview.last_content_area.y + (ctrl.line as u16),
                )
            }
            _ => unreachable!(),
        };
        dispatch(
            Action::Click {
                column: btn_x,
                row: btn_y,
            },
            &mut app,
            &mut ui,
        );

        // Verify DOM updated from "Pending" to "Approved"
        match ui.overlays.last_mut().unwrap() {
            Overlay::HtmlPreview(preview) => {
                let theme = HtmlPreviewOverlay::html_theme(&ui.theme);
                let r = preview.session.render(80, &theme, false);
                let text: String = r
                    .lines
                    .iter()
                    .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(text.contains("Approved"), "expected Approved in:\n{text}");
            }
            _ => unreachable!(),
        }

        // Toggle Raw mode via `m` (ContextMenuCurrent from keymap) and back
        dispatch(Action::ContextMenuCurrent, &mut app, &mut ui);
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::HtmlPreview(p)) if p.session.raw_mode
        ));
        dispatch(Action::Input('m'), &mut app, &mut ui);
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::HtmlPreview(p)) if !p.session.raw_mode
        ));

        // Press `o` -> native Axum serving via NativeHtmlServer::serve_html
        dispatch(Action::Input('o'), &mut app, &mut ui);
        let mut served_url = None;
        for _ in 0..10 {
            if let Some(msg) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                ui_rx.recv(),
            )
            .await
            .ok()
            .flatten()
            {
                if let crate::ui::app::UiMsg::HtmlServed { ref url } = msg {
                    served_url = Some(url.clone());
                }
                app.handle_ui_msg(&mut ui, msg);
                if served_url.is_some() {
                    break;
                }
            }
        }
        let url = served_url.expect("expected UiMsg::HtmlServed from NativeHtmlServer");
        assert!(url.starts_with("http://127.0.0.1:"));

        // Fetch the served URL over TCP and confirm 200 OK + HTML payload
        let without_scheme = url.strip_prefix("http://").unwrap();
        let (host_port, path_tail) = without_scheme.split_once('/').unwrap();
        let mut stream = TcpStream::connect(host_port).await.unwrap();
        let req = format!("GET /{path_tail} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "unexpected response: {resp}");
        assert!(resp.contains("Release Report"));
        assert!(resp.contains("Approve"));

        // Click outside modal -> closes HtmlPreview (dropping QuickJS session)
        dispatch(Action::Click { column: 0, row: 0 }, &mut app, &mut ui);
        assert!(ui.overlays.is_empty());

        app.html_server.shutdown();
        app.sync_ctrl.request(SyncCommand::Shutdown);
    }

    #[tokio::test]
    async fn image_preview_sniffs_html_bytes_and_auto_routes_to_html_preview() {
        use axum::{http::StatusCode, routing::get, Router};

        let mock_app = Router::new()
            .route(
                "/fake-image.png",
                get(|| async {
                    (
                        StatusCode::OK,
                        "<!DOCTYPE html><html><head><title>SSO Gate</title></head><body><h1>Sign in required</h1></body></html>",
                    )
                }),
            )
            .route(
                "/report.html",
                get(|| async {
                    (
                        StatusCode::OK,
                        "<!DOCTYPE html><html><head><title>Weekly HTML</title></head><body><p>Attached report</p></body></html>",
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, mock_app).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = format!("http://127.0.0.1:{}", addr.port());
        let auth = crate::auth::AuthManager::new(cfg.clone()).unwrap();
        let graph = crate::graph::GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = crate::storage::Storage::open_in_memory().unwrap();
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl =
            crate::sync::spawn_sync_engine(cfg.clone(), graph.clone(), storage.clone(), event_tx);
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        // 1. Image attachment whose URL actually serves HTML -> must auto-route to Overlay::HtmlPreview
        let fake_png_url = format!("http://127.0.0.1:{}/fake-image.png", addr.port());
        let html_att_url = format!("http://127.0.0.1:{}/report.html", addr.port());
        app.messages.push(crate::models::MessageItem {
            msg_key: "chat:c1|m-sniff".into(),
            conv_key: "chat:c1".into(),
            msg_id: "m-sniff".into(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Alice".into(),
            is_me: false,
            created_at: "2026-11-20T00:00:00Z".into(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".into(),
            message_type: "message".into(),
            body_html: String::new(),
            body_plain: String::new(),
            attachments: vec![
                crate::models::AttachmentInfo {
                    id: "hosted:fake1".into(),
                    content_type: "hostedContent".into(),
                    content_url: Some(fake_png_url),
                    name: Some("screenshot.png".into()),
                    content_json: None,
                    thumbnail_url: None,
                },
                crate::models::AttachmentInfo {
                    id: "att-html".into(),
                    content_type: "text/html".into(),
                    content_url: Some(html_att_url),
                    name: Some("report.html".into()),
                    content_json: None,
                    thumbnail_url: None,
                },
            ],
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        });
        ui.set_focus(Focus::Chat);
        ui.chat_scroll.selected_idx = Some(0);

        // Open ImagePreview for fake PNG attachment
        dispatch(Action::OpenImagePreview, &mut app, &mut ui);
        assert_eq!(
            ui.overlays.last().map(Overlay::kind),
            Some(OverlayKind::ImagePreview)
        );

        // Pump UiMsgs until ImageAutoRoutedToHtml switches overlay to HtmlPreview
        for _ in 0..10 {
            if let Some(msg) =
                tokio::time::timeout(std::time::Duration::from_secs(3), ui_rx.recv())
                    .await
                    .ok()
                    .flatten()
            {
                app.handle_ui_msg(&mut ui, msg);
                if ui.overlays.last().map(Overlay::kind) == Some(OverlayKind::HtmlPreview) {
                    break;
                }
            }
        }
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::HtmlPreview(p)) if p.raw_html().contains("Sign in required")
        ));

        // 2. Clicking an explicit `.html` attachment fetches and opens Overlay::HtmlPreview
        ui.overlays.clear();
        ui.hitmap.clear();
        ui.hitmap.add_line(
            4,
            6,
            20,
            HitTarget::Attachment {
                scope: MessageScope::Chat,
                msg_index: 0,
                attach_index: 1,
            },
        );
        dispatch(Action::Click { column: 6, row: 6 }, &mut app, &mut ui);
        for _ in 0..10 {
            if let Some(msg) =
                tokio::time::timeout(std::time::Duration::from_secs(3), ui_rx.recv())
                    .await
                    .ok()
                    .flatten()
            {
                app.handle_ui_msg(&mut ui, msg);
                if ui.overlays.last().map(Overlay::kind) == Some(OverlayKind::HtmlPreview) {
                    break;
                }
            }
        }
        assert!(matches!(
            ui.overlays.last(),
            Some(Overlay::HtmlPreview(p)) if p.raw_html().contains("Attached report")
        ));

        app.sync_ctrl.request(SyncCommand::Shutdown);
    }
}
