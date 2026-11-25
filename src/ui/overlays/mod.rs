//! Overlay stack & HelpOverlay (`?`).

pub mod attachments;
pub mod command_palette;
pub mod confirm;
pub mod context_menu;
pub mod file_picker;
pub mod html_preview;
pub mod image_preview;
pub mod links;
pub mod mentions;
pub mod new_chat;
pub mod path_input;
pub mod presence;
pub mod reactions;
pub mod room_info;
pub mod search;
pub mod settings;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::ui::{
    action::Action,
    app::{AppState, UiState},
    hitmap::{HitMap, HitTarget, MessageScope},
    overlays::{
        attachments::AttachmentsState, command_palette::CommandPalette, confirm::ConfirmDialog,
        context_menu::ContextMenu, file_picker::FilePickerOverlay,
        html_preview::HtmlPreviewOverlay, image_preview::ImagePreviewOverlay, links::LinksOverlay,
        new_chat::NewChatState, path_input::PathInputState, presence::PresenceOverlay,
        reactions::ReactionsOverlay, room_info::RoomInfoState, search::SearchOverlay,
        settings::SettingsState,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayKind {
    ContextMenu,
    CommandPalette,
    Settings,
    Search,
    Presence,
    Reactions,
    Attachments,
    PathInput,
    FilePicker,
    Links,
    NewChat,
    RoomInfo,
    ImagePreview,
    HtmlPreview,
    Help,
    Confirm,
}

impl OverlayKind {
    pub fn is_singleton(self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayOutcome {
    Handled,
    Close,
    Dispatch(Action),
    CloseAndDispatch(Action),
    NotHandled,
}

#[derive(Debug)]
pub enum Overlay {
    ContextMenu(ContextMenu),
    CommandPalette(CommandPalette),
    Settings(SettingsState),
    Search(SearchOverlay),
    Presence(PresenceOverlay),
    Reactions(ReactionsOverlay),
    Attachments(AttachmentsState),
    PathInput(PathInputState),
    FilePicker(FilePickerOverlay),
    Links(LinksOverlay),
    NewChat(NewChatState),
    RoomInfo(RoomInfoState),
    ImagePreview(ImagePreviewOverlay),
    HtmlPreview(HtmlPreviewOverlay),
    Help(HelpOverlay),
    Confirm(ConfirmDialog),
}

impl Overlay {
    pub fn kind(&self) -> OverlayKind {
        match self {
            Overlay::ContextMenu(_) => OverlayKind::ContextMenu,
            Overlay::CommandPalette(_) => OverlayKind::CommandPalette,
            Overlay::Settings(_) => OverlayKind::Settings,
            Overlay::Search(_) => OverlayKind::Search,
            Overlay::Presence(_) => OverlayKind::Presence,
            Overlay::Reactions(_) => OverlayKind::Reactions,
            Overlay::Attachments(_) => OverlayKind::Attachments,
            Overlay::PathInput(_) => OverlayKind::PathInput,
            Overlay::FilePicker(_) => OverlayKind::FilePicker,
            Overlay::Links(_) => OverlayKind::Links,
            Overlay::NewChat(_) => OverlayKind::NewChat,
            Overlay::RoomInfo(_) => OverlayKind::RoomInfo,
            Overlay::ImagePreview(_) => OverlayKind::ImagePreview,
            Overlay::HtmlPreview(_) => OverlayKind::HtmlPreview,
            Overlay::Help(_) => OverlayKind::Help,
            Overlay::Confirm(_) => OverlayKind::Confirm,
        }
    }

    pub fn accepts_text(&self) -> bool {
        match self {
            Overlay::Search(_)
            | Overlay::CommandPalette(_)
            | Overlay::PathInput(_)
            | Overlay::FilePicker(_)
            | Overlay::NewChat(_) => true,
            Overlay::HtmlPreview(preview) => preview.is_editing_input(),
            _ => false,
        }
    }

    pub fn html_preview(title: Option<String>, raw_html: String) -> Self {
        Overlay::HtmlPreview(HtmlPreviewOverlay::new(title, raw_html))
    }

    pub fn markdown_preview(title: Option<String>, raw_md: String) -> Self {
        Overlay::HtmlPreview(HtmlPreviewOverlay::new_markdown(title, raw_md))
    }

    pub fn context_menu(menu: ContextMenu) -> Self {
        Overlay::ContextMenu(menu)
    }

    pub fn command_palette() -> Self {
        Overlay::CommandPalette(CommandPalette::new())
    }

    pub fn settings() -> Self {
        Overlay::Settings(SettingsState::default())
    }

    pub fn search() -> Self {
        Overlay::Search(SearchOverlay::default())
    }

    pub fn presence() -> Self {
        Overlay::Presence(PresenceOverlay::default())
    }

    pub fn reactions(scope: MessageScope, msg_index: usize) -> Self {
        Overlay::Reactions(ReactionsOverlay::new(scope, msg_index))
    }

    pub fn attachments() -> Self {
        Overlay::Attachments(AttachmentsState::default())
    }

    pub fn path_input() -> Self {
        Overlay::PathInput(PathInputState::new())
    }

    pub fn file_picker() -> Self {
        Overlay::FilePicker(FilePickerOverlay::new())
    }

    pub fn links(overlay: LinksOverlay) -> Self {
        Overlay::Links(overlay)
    }

    pub fn new_chat() -> Self {
        Overlay::NewChat(NewChatState::new())
    }

    pub fn room_info() -> Self {
        Overlay::RoomInfo(RoomInfoState::default())
    }

    pub fn help() -> Self {
        Overlay::Help(HelpOverlay)
    }

    pub fn confirm(message: impl Into<String>, on_confirm: Option<Action>) -> Self {
        Overlay::Confirm(ConfirmDialog::new(message, on_confirm))
    }

    pub fn handle(&mut self, action: &Action, app: &AppState, ui: &UiState) -> OverlayOutcome {
        let wheel_step = match action {
            Action::MouseScroll { delta, .. } if *delta < 0 => Some(Action::MoveUp),
            Action::MouseScroll { delta, .. } if *delta > 0 => Some(Action::MoveDown),
            _ => None,
        };
        match self {
            Overlay::ContextMenu(menu) => match wheel_step.as_ref().unwrap_or(action) {
                Action::MoveUp => {
                    menu.prev();
                    OverlayOutcome::Handled
                }
                Action::MoveDown => {
                    menu.next();
                    OverlayOutcome::Handled
                }
                Action::Activate => menu.activate(),
                Action::CloseOverlay => OverlayOutcome::Close,
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::CommandPalette(palette) => match wheel_step.as_ref().unwrap_or(action) {
                Action::Tab => palette.handle(&Action::MoveDown, app, ui),
                Action::PrevFocus => palette.handle(&Action::MoveUp, app, ui),
                other => palette.handle(other, app, ui),
            },
            Overlay::Settings(settings) => match wheel_step.as_ref().unwrap_or(action) {
                Action::Tab => settings.handle(&Action::MoveDown, app),
                Action::PrevFocus => settings.handle(&Action::MoveUp, app),
                other => settings.handle(other, app),
            },
            Overlay::Search(search) => search.handle(wheel_step.as_ref().unwrap_or(action), app),
            Overlay::Presence(presence) => presence.handle(wheel_step.as_ref().unwrap_or(action)),
            Overlay::Reactions(reactions) => {
                reactions.handle(wheel_step.as_ref().unwrap_or(action))
            }
            Overlay::Attachments(tray) => tray.handle(wheel_step.as_ref().unwrap_or(action), app),
            Overlay::PathInput(input) => input.handle(action),
            Overlay::FilePicker(picker) => picker.handle(wheel_step.as_ref().unwrap_or(action)),
            Overlay::Links(links) => links.handle(wheel_step.as_ref().unwrap_or(action)),
            Overlay::NewChat(new_chat) => match wheel_step.as_ref().unwrap_or(action) {
                Action::Tab => new_chat.handle(&Action::MoveDown, app),
                Action::PrevFocus => new_chat.handle(&Action::MoveUp, app),
                other => new_chat.handle(other, app),
            },
            Overlay::RoomInfo(info) => info.handle(wheel_step.as_ref().unwrap_or(action)),
            Overlay::ImagePreview(preview) => preview.handle(action),
            Overlay::HtmlPreview(preview) => preview.handle(action),
            Overlay::Help(_) => match action {
                Action::CloseOverlay | Action::Activate => OverlayOutcome::Close,
                Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
                _ => OverlayOutcome::Handled,
            },
            Overlay::Confirm(dialog) => dialog.handle(action),
        }
    }

    pub fn select(&mut self, index: usize) {
        match self {
            Overlay::ContextMenu(menu) => menu.select(index),
            Overlay::CommandPalette(palette) => palette.select(index),
            Overlay::Settings(settings) => settings.select(index),
            Overlay::Search(search) => search.select(index),
            Overlay::Presence(presence) => presence.select(index),
            Overlay::Reactions(reactions) => reactions.select(index),
            Overlay::Attachments(tray) => tray.select(index),
            Overlay::FilePicker(picker) => picker.select(index),
            Overlay::Links(links) => links.select(index),
            Overlay::NewChat(new_chat) => new_chat.select(index),
            Overlay::RoomInfo(info) => info.select(index),
            Overlay::ImagePreview(_)
            | Overlay::HtmlPreview(_)
            | Overlay::PathInput(_)
            | Overlay::Help(_)
            | Overlay::Confirm(_) => {}
        }
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        app: &AppState,
        ui: &UiState,
        hitmap: &mut HitMap,
    ) {
        match self {
            Overlay::ContextMenu(menu) => menu.draw(frame, area, ui, hitmap),
            Overlay::CommandPalette(palette) => palette.draw(frame, area, app, ui, hitmap),
            Overlay::Settings(settings) => settings.draw(frame, area, app, ui, hitmap),
            Overlay::Search(search) => search.draw(frame, area, app, ui, hitmap),
            Overlay::Presence(presence) => presence.draw(frame, area, app, ui, hitmap),
            Overlay::Reactions(reactions) => reactions.draw(frame, area, app, ui, hitmap),
            Overlay::Attachments(tray) => tray.draw(frame, area, app, ui, hitmap),
            Overlay::PathInput(input) => input.draw(frame, area, ui, hitmap),
            Overlay::FilePicker(picker) => picker.draw(frame, area, ui, hitmap),
            Overlay::Links(links) => links.draw(frame, area, ui, hitmap),
            Overlay::NewChat(new_chat) => new_chat.draw(frame, area, app, ui, hitmap),
            Overlay::RoomInfo(info) => info.draw(frame, area, app, ui, hitmap),
            Overlay::ImagePreview(preview) => preview.draw(frame, area, app, ui, hitmap),
            Overlay::HtmlPreview(preview) => preview.draw_readonly(frame, area, ui, hitmap),
            Overlay::Help(_) => draw_help(frame, area, ui, hitmap),
            Overlay::Confirm(dialog) => dialog.draw(frame, area, ui, hitmap),
        }
    }
}

// ---------------------------------------------------------------------------
// Help Overlay
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct HelpOverlay;

const HELP_SECTIONS: [(&str, &[(&str, &str)]); 4] = [
    (
        "Navigation & Switching",
        &[
            ("Ctrl+K", "Omni-Switcher (chats, channels, commands)"),
            ("1/2/3 · Alt+1..3", "Chats / Teams / Activity (@mentions)"),
            ("u (Nav) / U / Alt+u", "Jump to next / previous unread"),
            ("` / Alt+← / Alt+→", "Last conversation / history back-fwd"),
            ("Alt+↑ / Alt+↓", "Previous / next conversation in list"),
            ("←/→ or h/l (Nav)", "Collapse / expand team or open chat"),
            ("p / f (Nav)", "Pin conversation / filter unread only"),
            ("Tab / Shift+Tab", "Cycle pane focus"),
        ],
    ),
    (
        "Messages & Threads",
        &[
            ("↑/↓, j/k", "Select messages / navigate list"),
            ("PgUp / PgDn", "Scroll history (stays in Composer)"),
            ("Ctrl+↑ / Ctrl+↓", "Scroll 3 lines from Composer"),
            ("o / s", "Load older (50) / deep history sync"),
            ("t / + / e", "Open channel thread / add reaction"),
            ("r / y / Del", "Reply / copy / delete message"),
            ("u / h", "Extract links / toggle inline href URLs"),
            ("v / H / d / m", "Preview image / HTML / download / menu"),
        ],
    ),
    (
        "Composing & Search",
        &[
            ("i / Enter", "Focus Composer / send message"),
            ("Shift+Enter / Ctrl+E", "Newline / external $EDITOR"),
            ("Ctrl+W / Alt+Bksp", "Delete word backward"),
            ("Ctrl+V / Ctrl+Shift+X", "Paste image/text / clear pending images"),
            ("Alt+T / Ctrl+S", "Toggle thread target / channel subject"),
            ("Ctrl+F / Ctrl+O / Ctrl+T", "File picker (zoxide) / path / tray"),
            ("/", "Search messages"),
        ],
    ),
    (
        "Window & Preferences",
        &[
            ("Ctrl+D", "Toggle density (Cozy / Compact)"),
            ("Ctrl+N / Ctrl+U", "New chat / room info & members"),
            ("Ctrl+P / Ctrl+,", "Set presence / settings"),
            ("Ctrl+B", "Toggle left navigation rail"),
            ("F5 / R", "Refresh conversations"),
            ("? / Ctrl+Q", "Help / quit (confirms unsent draft)"),
        ],
    ),
];

fn draw_help(frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
    let theme = &ui.theme;
    let rect = centered_rect(
        area,
        area.width.saturating_sub(6).min(96),
        area.height.saturating_sub(3).min(24),
    );
    frame.render_widget(Clear, rect);
    hitmap.add(rect, HitTarget::OverlayBody);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
        .title(Line::from(Span::styled(
            " Microsoft Teams TUI Help · Esc Close ",
            theme.title(true),
        )))
        .border_style(theme.border_style(true));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(inner);

    for (index, (title, entries)) in HELP_SECTIONS.iter().enumerate() {
        let target = columns[index % 2];
        if target.width == 0 {
            continue;
        }
        let y_offset = if index < 2 { 0 } else { target.height / 2 };
        let mut lines = Vec::with_capacity(entries.len() + 1);
        lines.push(Line::from(Span::styled(
            format!(" {title}"),
            theme.title(true),
        )));
        for (key, desc) in entries.iter() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<21}"), theme.accent_style()),
                Span::raw(desc.to_string()),
            ]));
        }
        let sub = Rect::new(
            target.x,
            target.y + y_offset,
            target.width,
            target.height.saturating_sub(y_offset),
        );
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), sub);
    }
}

pub fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

pub fn draw_stack(
    frame: &mut Frame,
    area: Rect,
    app: &AppState,
    ui: &mut UiState,
    hitmap: &mut HitMap,
) {
    if let Some(mut overlay) = ui.overlays.pop() {
        overlay.draw(frame, area, app, ui, hitmap);
        ui.overlays.push(overlay);
    }
}
