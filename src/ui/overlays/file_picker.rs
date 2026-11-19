//! Interactive local File Picker overlay (`Ctrl+F`) with `zoxide` quick-jump (`z`).
//! Supports Up/Down navigation, Enter to enter directory or attach file, Backspace for parent dir,
//! type-to-filter, and `z` (when filter is empty) to jump via `zoxide query -l`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    Frame,
};

use crate::ui::{
    action::Action,
    app::UiState,
    hitmap::{HitMap, HitTarget},
    overlays::{centered_rect, OverlayOutcome},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePickerEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerMode {
    #[default]
    Browse,
    ZoxideJump,
}

#[derive(Debug, Clone)]
pub struct FilePickerOverlay {
    pub current_dir: PathBuf,
    pub entries: Vec<FilePickerEntry>,
    pub filter: String,
    pub selected: usize,
    pub mode: PickerMode,
    pub zoxide_dirs: Vec<PathBuf>,
    pub zoxide_filter: String,
    pub zoxide_selected: usize,
    pub status: Option<String>,
}

impl Default for FilePickerOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl FilePickerOverlay {
    pub fn new() -> Self {
        let start = std::env::current_dir()
            .ok()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::open_dir(start)
    }

    pub fn open_dir(dir: PathBuf) -> Self {
        let mut picker = Self {
            current_dir: dir,
            entries: Vec::new(),
            filter: String::new(),
            selected: 0,
            mode: PickerMode::Browse,
            zoxide_dirs: Vec::new(),
            zoxide_filter: String::new(),
            zoxide_selected: 0,
            status: None,
        };
        picker.reload_entries();
        picker
    }

    pub fn reload_entries(&mut self) {
        self.entries.clear();
        self.selected = 0;
        match std::fs::read_dir(&self.current_dir) {
            Ok(read_dir) => {
                let mut dirs_vec = Vec::new();
                let mut files_vec = Vec::new();
                for entry in read_dir.flatten() {
                    let path = entry.path();
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let meta = entry.metadata().ok();
                    let is_dir = meta
                        .as_ref()
                        .map(|m| m.is_dir())
                        .unwrap_or_else(|| path.is_dir());
                    let size = if is_dir {
                        None
                    } else {
                        meta.as_ref().map(|m| m.len())
                    };
                    let item = FilePickerEntry {
                        name,
                        path,
                        is_dir,
                        size,
                    };
                    if is_dir {
                        dirs_vec.push(item);
                    } else {
                        files_vec.push(item);
                    }
                }
                dirs_vec.sort_by(|a, b| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                        .then_with(|| a.name.cmp(&b.name))
                });
                files_vec.sort_by(|a, b| {
                    a.name
                        .to_ascii_lowercase()
                        .cmp(&b.name.to_ascii_lowercase())
                        .then_with(|| a.name.cmp(&b.name))
                });
                self.entries.extend(dirs_vec);
                self.entries.extend(files_vec);
                self.status = None;
            }
            Err(err) => {
                self.status = Some(format!("Cannot read directory: {err}"));
            }
        }
    }

    pub fn filtered_entries(&self) -> Vec<&FilePickerEntry> {
        let needle = self.filter.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return self.entries.iter().collect();
        }
        self.entries
            .iter()
            .filter(|e| e.name.to_ascii_lowercase().contains(&needle))
            .collect()
    }

    pub fn filtered_zoxide_dirs(&self) -> Vec<&PathBuf> {
        let needle = self.zoxide_filter.trim().to_ascii_lowercase();
        if needle.is_empty() {
            return self.zoxide_dirs.iter().collect();
        }
        self.zoxide_dirs
            .iter()
            .filter(|p| p.to_string_lossy().to_ascii_lowercase().contains(&needle))
            .collect()
    }

    pub fn enter_zoxide_mode_with(&mut self, dirs: Vec<PathBuf>) -> bool {
        if dirs.is_empty() {
            self.status = Some("zoxide not available or has no indexed directories".to_string());
            return false;
        }
        self.zoxide_dirs = dirs;
        self.zoxide_filter.clear();
        self.zoxide_selected = 0;
        self.mode = PickerMode::ZoxideJump;
        self.status = None;
        true
    }

    pub fn go_parent(&mut self) {
        if let Some(parent) = self.current_dir.parent().map(Path::to_path_buf) {
            if parent != self.current_dir {
                self.current_dir = parent;
                self.filter.clear();
                self.reload_entries();
            }
        }
    }

    pub fn select(&mut self, index: usize) {
        match self.mode {
            PickerMode::Browse => {
                let len = self.filtered_entries().len();
                if index < len {
                    self.selected = index;
                }
            }
            PickerMode::ZoxideJump => {
                let len = self.filtered_zoxide_dirs().len();
                if index < len {
                    self.zoxide_selected = index;
                }
            }
        }
    }

    pub fn handle(&mut self, action: &Action) -> OverlayOutcome {
        match self.mode {
            PickerMode::Browse => self.handle_browse(action),
            PickerMode::ZoxideJump => self.handle_zoxide(action),
        }
    }

    fn handle_browse(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp => {
                let len = self.filtered_entries().len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                let len = self.filtered_entries().len();
                if len > 0 {
                    let cur = self.selected.min(len - 1);
                    self.selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let chosen = {
                    let list = self.filtered_entries();
                    if list.is_empty() {
                        None
                    } else {
                        let idx = self.selected.min(list.len() - 1);
                        Some(list[idx].clone())
                    }
                };
                if let Some(entry) = chosen {
                    if entry.is_dir {
                        self.current_dir = entry.path;
                        self.filter.clear();
                        self.reload_entries();
                        OverlayOutcome::Handled
                    } else {
                        OverlayOutcome::CloseAndDispatch(Action::AttachPickedFile(
                            entry.path.to_string_lossy().into_owned(),
                        ))
                    }
                } else {
                    OverlayOutcome::Handled
                }
            }
            Action::Backspace => {
                if self.filter.pop().is_some() {
                    self.selected = 0;
                    self.status = None;
                } else {
                    self.go_parent();
                }
                OverlayOutcome::Handled
            }
            Action::Input('z') if self.filter.is_empty() => {
                let dirs = query_zoxide_dirs();
                self.enter_zoxide_mode_with(dirs);
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.filter.push(*ch);
                self.selected = 0;
                self.status = None;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                let clean = text.replace(['\r', '\n'], "");
                if !clean.is_empty() {
                    self.filter.push_str(&clean);
                    self.selected = 0;
                    self.status = None;
                }
                OverlayOutcome::Handled
            }
            Action::CloseOverlay => OverlayOutcome::Close,
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    fn handle_zoxide(&mut self, action: &Action) -> OverlayOutcome {
        match action {
            Action::MoveUp => {
                let len = self.filtered_zoxide_dirs().len();
                if len > 0 {
                    let cur = self.zoxide_selected.min(len - 1);
                    self.zoxide_selected = (cur + len - 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::MoveDown => {
                let len = self.filtered_zoxide_dirs().len();
                if len > 0 {
                    let cur = self.zoxide_selected.min(len - 1);
                    self.zoxide_selected = (cur + 1) % len;
                }
                OverlayOutcome::Handled
            }
            Action::Activate => {
                let chosen = {
                    let list = self.filtered_zoxide_dirs();
                    if list.is_empty() {
                        None
                    } else {
                        let idx = self.zoxide_selected.min(list.len() - 1);
                        Some(list[idx].clone())
                    }
                };
                if let Some(dir) = chosen {
                    self.current_dir = dir;
                    self.filter.clear();
                    self.mode = PickerMode::Browse;
                    self.reload_entries();
                } else {
                    self.mode = PickerMode::Browse;
                }
                OverlayOutcome::Handled
            }
            Action::Backspace => {
                if self.zoxide_filter.pop().is_some() {
                    self.zoxide_selected = 0;
                } else {
                    self.mode = PickerMode::Browse;
                }
                OverlayOutcome::Handled
            }
            Action::Input(ch) => {
                self.zoxide_filter.push(*ch);
                self.zoxide_selected = 0;
                OverlayOutcome::Handled
            }
            Action::Paste(text) => {
                let clean = text.replace(['\r', '\n'], "");
                if !clean.is_empty() {
                    self.zoxide_filter.push_str(&clean);
                    self.zoxide_selected = 0;
                }
                OverlayOutcome::Handled
            }
            Action::CloseOverlay => {
                self.mode = PickerMode::Browse;
                OverlayOutcome::Handled
            }
            Action::Quit | Action::QuitConfirm => OverlayOutcome::NotHandled,
            _ => OverlayOutcome::Handled,
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect, ui: &UiState, hitmap: &mut HitMap) {
        let theme = &ui.theme;
        let rect = centered_rect(
            area,
            area.width.saturating_sub(6).min(86),
            area.height.saturating_sub(4).min(24),
        );
        frame.render_widget(Clear, rect);
        hitmap.add(rect, HitTarget::OverlayBody);

        let title = match self.mode {
            PickerMode::Browse => {
                " File Picker · Enter Open/Attach · Bksp Up · z zoxide · Esc Close "
            }
            PickerMode::ZoxideJump => " zoxide Jump · Enter Jump · Type to Filter · Esc Back ",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(theme.bg_elevated).fg(theme.fg))
            .title(Line::from(Span::styled(
                crate::ui::truncate_width(title, rect.width.saturating_sub(2) as usize),
                theme.title(true),
            )))
            .border_style(theme.border_style(true));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        // Row 0: Current path or zoxide prompt
        let header_line = match self.mode {
            PickerMode::Browse => {
                let dir_str = self.current_dir.display().to_string();
                let prefix = " Dir: ";
                let max_dir_w = (inner.width as usize).saturating_sub(prefix.len());
                Line::from(vec![
                    Span::styled(prefix, theme.muted_style()),
                    Span::styled(
                        crate::ui::truncate_width(&dir_str, max_dir_w),
                        theme.accent_style().add_modifier(Modifier::BOLD),
                    ),
                ])
            }
            PickerMode::ZoxideJump => Line::from(vec![
                Span::styled(" zoxide > ", theme.title(true)),
                Span::raw(self.zoxide_filter.clone()),
                Span::styled("▏", theme.muted_style()),
            ]),
        };
        frame.render_widget(
            Paragraph::new(header_line),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        // Row 1: Filter bar (Browse mode) or separator
        let list_start_y = if self.mode == PickerMode::Browse && inner.height >= 3 {
            let filter_line = if self.filter.is_empty() {
                Line::from(vec![
                    Span::styled(" Filter: ", theme.muted_style()),
                    Span::styled(
                        "(type to filter, or press z for zoxide)",
                        theme.muted_style().add_modifier(Modifier::ITALIC),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(" Filter: ", theme.title(true)),
                    Span::raw(self.filter.clone()),
                    Span::styled("▏", theme.accent_style()),
                ])
            };
            frame.render_widget(
                Paragraph::new(filter_line),
                Rect::new(inner.x, inner.y + 1, inner.width, 1),
            );
            inner.y + 2
        } else {
            inner.y + 1
        };

        let has_status = self.status.is_some() && inner.bottom() > list_start_y + 1;
        let list_bottom = if has_status {
            inner.bottom().saturating_sub(1)
        } else {
            inner.bottom()
        };
        let visible = list_bottom.saturating_sub(list_start_y) as usize;

        if visible > 0 {
            match self.mode {
                PickerMode::Browse => {
                    let items = self.filtered_entries();
                    if items.is_empty() {
                        let empty_msg = if self.filter.is_empty() {
                            " (empty directory)"
                        } else {
                            " (no matching files or folders)"
                        };
                        frame.render_widget(
                            Paragraph::new(Span::styled(empty_msg, theme.muted_style())),
                            Rect::new(inner.x, list_start_y, inner.width, 1),
                        );
                    } else {
                        let active_sel = self.selected.min(items.len() - 1);
                        let skip = active_sel.saturating_sub(visible.saturating_sub(1));
                        for (row, (idx, entry)) in items
                            .iter()
                            .enumerate()
                            .skip(skip)
                            .take(visible)
                            .enumerate()
                        {
                            let y = list_start_y + row as u16;
                            let is_sel = idx == active_sel;
                            let style = if is_sel {
                                theme.selection()
                            } else if entry.is_dir {
                                theme.accent_style().add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            };
                            let size_label = entry
                                .size
                                .map(format_file_size)
                                .unwrap_or_else(|| "DIR".to_string());
                            let right_w = size_label.len().max(6);
                            let name_display = if entry.is_dir {
                                format!("{}/", entry.name)
                            } else {
                                entry.name.clone()
                            };
                            let label_w = (inner.width as usize).saturating_sub(right_w + 4).max(8);
                            let trunc = crate::ui::truncate_width(&name_display, label_w);
                            let used = unicode_width::UnicodeWidthStr::width(trunc.as_str());
                            let pad = label_w.saturating_sub(used);
                            let line = Line::from(vec![
                                Span::styled(format!(" {trunc}{}", " ".repeat(pad)), style),
                                Span::styled(
                                    format!(" {size_label:>width$} ", width = right_w),
                                    if is_sel {
                                        theme.selection()
                                    } else {
                                        theme.muted_style()
                                    },
                                ),
                            ]);
                            frame.render_widget(
                                Paragraph::new(line),
                                Rect::new(inner.x, y, inner.width, 1),
                            );
                            hitmap.add_line(
                                inner.x,
                                y,
                                inner.width,
                                HitTarget::OverlayItem { index: idx },
                            );
                        }
                    }
                }
                PickerMode::ZoxideJump => {
                    let dirs = self.filtered_zoxide_dirs();
                    if dirs.is_empty() {
                        frame.render_widget(
                            Paragraph::new(Span::styled(
                                " (no matching zoxide directories)",
                                theme.muted_style(),
                            )),
                            Rect::new(inner.x, list_start_y, inner.width, 1),
                        );
                    } else {
                        let active_sel = self.zoxide_selected.min(dirs.len() - 1);
                        let skip = active_sel.saturating_sub(visible.saturating_sub(1));
                        for (row, (idx, dir)) in dirs
                            .iter()
                            .enumerate()
                            .skip(skip)
                            .take(visible)
                            .enumerate()
                        {
                            let y = list_start_y + row as u16;
                            let is_sel = idx == active_sel;
                            let style = if is_sel {
                                theme.selection()
                            } else {
                                Style::default()
                            };
                            let text = format!(" {}", dir.display());
                            frame.render_widget(
                                Paragraph::new(Line::from(Span::styled(
                                    crate::ui::truncate_width(&text, inner.width as usize),
                                    style,
                                ))),
                                Rect::new(inner.x, y, inner.width, 1),
                            );
                            hitmap.add_line(
                                inner.x,
                                y,
                                inner.width,
                                HitTarget::OverlayItem { index: idx },
                            );
                        }
                    }
                }
            }
        }

        if has_status {
            if let Some(ref msg) = self.status {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        crate::ui::truncate_width(&format!(" {msg}"), inner.width as usize),
                        Style::default().fg(theme.warning),
                    )),
                    Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                );
            }
        }
    }
}

/// Parses newline-delimited directory paths returned by `zoxide query -l`.
pub fn parse_zoxide_output(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn query_zoxide_dirs() -> Vec<PathBuf> {
    let Ok(output) = Command::new("zoxide")
        .args(["query", "-l"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_zoxide_output(&text)
}

fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn browse_filter_enter_dir_parent_and_select_file() {
        let root = tempdir().unwrap();
        let sub = root.path().join("alpha_dir");
        std::fs::create_dir(&sub).unwrap();
        let nested_file = sub.join("report.pdf");
        std::fs::write(&nested_file, b"pdf-bytes").unwrap();
        let top_file = root.path().join("notes.txt");
        std::fs::write(&top_file, b"hello").unwrap();

        let mut picker = FilePickerOverlay::open_dir(root.path().to_path_buf());
        // Directories sort before files.
        assert_eq!(picker.entries.len(), 2);
        assert!(picker.entries[0].is_dir);
        assert_eq!(picker.entries[0].name, "alpha_dir");
        assert!(!picker.entries[1].is_dir);
        assert_eq!(picker.entries[1].name, "notes.txt");

        // Enter into directory
        assert_eq!(picker.handle(&Action::Activate), OverlayOutcome::Handled);
        assert_eq!(picker.current_dir, sub);
        assert_eq!(picker.entries.len(), 1);
        assert_eq!(picker.entries[0].name, "report.pdf");

        // Type-to-filter matches report.pdf; Backspace clears filter first, then goes to parent
        picker.handle(&Action::Input('r'));
        assert_eq!(picker.filter, "r");
        assert_eq!(picker.filtered_entries().len(), 1);
        picker.handle(&Action::Backspace);
        assert!(picker.filter.is_empty());
        assert_eq!(picker.current_dir, sub);

        // Enter on file dispatches AttachPickedFile
        let outcome = picker.handle(&Action::Activate);
        assert_eq!(
            outcome,
            OverlayOutcome::CloseAndDispatch(Action::AttachPickedFile(
                nested_file.to_string_lossy().into_owned()
            ))
        );

        // Backspace with empty filter goes to parent directory
        picker.handle(&Action::Backspace);
        assert_eq!(picker.current_dir, root.path());
    }

    #[test]
    fn zoxide_output_parsing_and_jump_mode() {
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        std::fs::write(dir_b.path().join("spec.md"), b"# spec").unwrap();

        let raw = format!("{}\n\n{}\n", dir_a.path().display(), dir_b.path().display());
        let parsed = parse_zoxide_output(&raw);
        assert_eq!(parsed.len(), 2);

        let mut picker = FilePickerOverlay::open_dir(dir_a.path().to_path_buf());
        assert!(picker.enter_zoxide_mode_with(parsed));
        assert_eq!(picker.mode, PickerMode::ZoxideJump);

        picker.handle(&Action::MoveDown);
        assert_eq!(picker.zoxide_selected, 1);

        assert_eq!(picker.handle(&Action::Activate), OverlayOutcome::Handled);
        assert_eq!(picker.mode, PickerMode::Browse);
        assert_eq!(picker.current_dir, dir_b.path());
        assert!(picker.entries.iter().any(|e| e.name == "spec.md"));
    }
}
