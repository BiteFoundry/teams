//! Inline terminal image preview renderer using `image` (PNG/JPEG/GIF/WebP) ->
//! Truecolor Unicode Half-Blocks (`▀` U+2580) for `ratatui` + Kitty / iTerm2 / Sixel / ANSI Half-Block rendering for TUI & CLI.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, IsTerminal};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use image::{imageops::FilterType, DynamicImage, ImageFormat, Rgba};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
};

use crate::config::ImageProtocolPreference;

// App-specific environment variable keys (the ONLY lines that differ across BiteFoundry apps).
const ENV_PROTOCOL_KEYS: &[&str] = &["TEAMS_IMAGE_PROTOCOL"];
const ENV_CELL_PX_KEYS: &[&str] = &["TEAMS_CELL_PIXEL_SIZE"];

/// Upper memory budget (8 MiB) for decoded terminal inline image caches.
pub const MAX_IMAGE_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Fixed Kitty graphics image ID used by the TUI preview overlay so prior placements can be cleanly deleted.
pub const KITTY_PREVIEW_IMAGE_ID: u32 = 4242;

/// Single memory-safety upper bound on rendered / transmitted pixel dimensions (supports 4K/HiDPI & 300% zoom).
pub const MAX_RENDER_PX_W: u32 = 4096;
pub const MAX_RENDER_PX_H: u32 = 8192;
/// Proportional pixel-area guard for Sixel RGBA quantization buffers.
pub const MAX_SIXEL_PIXELS: u64 = 4_000_000;

const MAX_ITERM2_RAW_BYTES: usize = 2 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_millis(150);
const MAX_QUANT_LOOKUP_CACHE: usize = 16_384;

/// Explicit sizing contract for terminal image layout (replaces implicit `max_rows >= max_cols` signaling).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FitBox {
    pub view_cols: u16,
    pub view_rows: u16,
    pub zoom_pct: u16,
    pub scrollable: bool,
}

impl FitBox {
    pub fn new(view_cols: u16, view_rows: u16, zoom_pct: u16, scrollable: bool) -> Self {
        Self {
            view_cols: view_cols.max(1),
            view_rows: view_rows.max(1),
            zoom_pct: zoom_pct.clamp(25, 300),
            scrollable,
        }
    }

    pub fn fixed(view_cols: u16, view_rows: u16) -> Self {
        Self {
            view_cols: view_cols.max(1),
            view_rows: view_rows.max(1),
            zoom_pct: 100,
            scrollable: false,
        }
    }
}

/// Constraining axis emitted to Kitty Graphics Protocol (`c=` OR `r=`, never both simultaneously).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KittyFitAxis {
    Cols(u16),
    Rows(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminalImageProtocol {
    Kitty,
    ITerm2,
    Sixel,
    Halfblock,
}

impl TerminalImageProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Kitty => "kitty",
            Self::ITerm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Kitty => "Kitty",
            Self::ITerm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }

    pub fn is_pixel(self) -> bool {
        !matches!(self, Self::Halfblock)
    }

    pub fn next_manual(self) -> Self {
        match self {
            Self::Kitty => Self::ITerm2,
            Self::ITerm2 => Self::Sixel,
            Self::Sixel => Self::Halfblock,
            Self::Halfblock => Self::Kitty,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalGraphicsCaps {
    pub protocol: TerminalImageProtocol,
    pub cell_px: (u16, u16),
    pub cell_px_exact: bool,
}

impl Default for TerminalGraphicsCaps {
    fn default() -> Self {
        Self {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: default_cell_pixel_size(),
            cell_px_exact: false,
        }
    }
}

fn default_cell_pixel_size() -> (u16, u16) {
    #[cfg(windows)]
    {
        (10, 20)
    }
    #[cfg(not(windows))]
    {
        (8, 16)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PixelScrollBacking {
    SixelIndexed {
        width: u32,
        height: u32,
        cell_px: (u16, u16),
        palette: Vec<(u8, u8, u8)>,
        indexed: Arc<Vec<u8>>,
    },
    Source {
        image: Arc<DynamicImage>,
        target_px_w: u32,
        target_px_h: u32,
        cell_px: (u16, u16),
    },
}

impl PixelScrollBacking {
    fn approx_heap_bytes(&self) -> usize {
        match self {
            Self::SixelIndexed {
                palette, indexed, ..
            } => palette.capacity() * std::mem::size_of::<(u8, u8, u8)>() + indexed.capacity(),
            Self::Source { image, .. } => (image.width() as usize)
                .saturating_mul(image.height() as usize)
                .saturating_mul(image.color().bytes_per_pixel() as usize),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RenderedTerminalImage {
    pub width_px: u32,
    pub height_px: u32,
    pub cols: u16,
    pub rows: u16,
    pub format_label: String,
    pub protocol: TerminalImageProtocol,
    pub lines: Vec<Line<'static>>,
    /// Pre-encoded escape sequence payload when `protocol.is_pixel()` is true (positioned by caller in TUI).
    pub pixel_payload: Option<String>,
    /// Optional scrollable pixel backing used to generate viewport-cropped slices on scroll.
    pub(crate) scroll_backing: Option<Box<PixelScrollBacking>>,
}

impl RenderedTerminalImage {
    /// Approximate heap footprint of this rendered terminal image in bytes.
    pub fn approx_heap_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>()
            + self.format_label.capacity()
            + self.lines.capacity() * std::mem::size_of::<Line<'static>>()
            + self.pixel_payload.as_ref().map_or(0, |s| s.capacity())
            + self
                .scroll_backing
                .as_deref()
                .map_or(0, |b| std::mem::size_of::<PixelScrollBacking>() + b.approx_heap_bytes());
        for line in &self.lines {
            total += line.spans.capacity() * std::mem::size_of::<Span<'static>>();
            for span in &line.spans {
                total += span.content.len();
            }
        }
        total
    }

    /// Returns a viewport-cropped pixel protocol payload `(payload, draw_cols, draw_rows)` guaranteed
    /// to fit within `(view_cols, view_rows)` cells starting at cell offset `(scroll_x, scroll_y)`.
    pub fn viewport_pixel_payload(
        &self,
        scroll_x: u16,
        scroll_y: u16,
        view_cols: u16,
        view_rows: u16,
    ) -> Option<(String, u16, u16)> {
        if !self.protocol.is_pixel() || view_cols == 0 || view_rows == 0 {
            return None;
        }
        if scroll_x == 0 && scroll_y == 0 && self.cols <= view_cols && self.rows <= view_rows {
            return Some((self.pixel_payload.clone()?, self.cols, self.rows));
        }

        let backing = self.scroll_backing.as_deref()?;
        match backing {
            PixelScrollBacking::SixelIndexed {
                width,
                height,
                cell_px,
                palette,
                indexed,
            } => {
                let cell_w = cell_px.0.max(1) as u32;
                let cell_h = cell_px.1.max(1) as u32;
                let x0 = (scroll_x as u32).saturating_mul(cell_w);
                let y0 = (scroll_y as u32).saturating_mul(cell_h);
                if x0 >= *width || y0 >= *height {
                    return None;
                }
                let max_w = (view_cols as u32).saturating_mul(cell_w);
                let raw_max_h = (view_rows as u32).saturating_mul(cell_h);
                let safe_max_h = compute_sixel_safe_pixel_height(view_rows, cell_px.1.max(1));
                let max_h = if safe_max_h >= 6 {
                    safe_max_h.min(raw_max_h)
                } else {
                    raw_max_h
                };
                let crop_w = (*width - x0).min(max_w);
                let mut crop_h = (*height - y0).min(max_h);
                // Sixel emits 6-pixel vertical bands; keep band-rounded height <= max_h
                // so terminals that paint full 6px bands never spill past view_rows.
                if crop_h.div_ceil(6) * 6 > max_h && max_h >= 6 {
                    crop_h = max_h - (max_h % 6);
                }
                if crop_w == 0 || crop_h == 0 {
                    return None;
                }
                let draw_cols = crop_w.div_ceil(cell_w).clamp(1, view_cols as u32) as u16;
                let draw_rows = crop_h.div_ceil(cell_h).clamp(1, view_rows as u32) as u16;
                let payload = encode_sixel_indexed_slice(
                    palette,
                    indexed,
                    *width as usize,
                    x0 as usize,
                    y0 as usize,
                    crop_w as usize,
                    crop_h as usize,
                );
                Some((payload, draw_cols, draw_rows))
            }
            PixelScrollBacking::Source {
                image,
                target_px_w,
                target_px_h,
                cell_px,
            } => {
                let cell_w = cell_px.0.max(1) as u32;
                let cell_h = cell_px.1.max(1) as u32;
                let x0 = (scroll_x as u32).saturating_mul(cell_w);
                let y0 = (scroll_y as u32).saturating_mul(cell_h);
                if x0 >= *target_px_w || y0 >= *target_px_h {
                    return None;
                }
                let max_w = (view_cols as u32).saturating_mul(cell_w);
                let max_h = (view_rows as u32).saturating_mul(cell_h);
                let crop_w = (*target_px_w - x0).min(max_w);
                let crop_h = (*target_px_h - y0).min(max_h);
                if crop_w == 0 || crop_h == 0 {
                    return None;
                }
                let draw_cols = crop_w.div_ceil(cell_w).clamp(1, view_cols as u32) as u16;
                let draw_rows = crop_h.div_ceil(cell_h).clamp(1, view_rows as u32) as u16;

                let src_w = image.width();
                let src_h = image.height();
                if src_w == 0 || src_h == 0 || *target_px_w == 0 || *target_px_h == 0 {
                    return None;
                }
                let src_x0 = ((x0 as u64) * (src_w as u64) / (*target_px_w as u64))
                    .min((src_w - 1) as u64) as u32;
                let src_y0 = ((y0 as u64) * (src_h as u64) / (*target_px_h as u64))
                    .min((src_h - 1) as u64) as u32;
                let src_crop_w = ((crop_w as u64) * (src_w as u64))
                    .div_ceil(*target_px_w as u64)
                    .clamp(1, (src_w - src_x0) as u64) as u32;
                let src_crop_h = ((crop_h as u64) * (src_h as u64))
                    .div_ceil(*target_px_h as u64)
                    .clamp(1, (src_h - src_y0) as u64) as u32;

                let sub_img = image.crop_imm(src_x0, src_y0, src_crop_w, src_crop_h);
                let png_bytes = encode_png(&sub_img).ok()?;
                let payload = match self.protocol {
                    TerminalImageProtocol::Kitty => {
                        let axis = choose_kitty_fit_axis(
                            src_crop_w,
                            src_crop_h,
                            draw_cols,
                            draw_rows,
                            *cell_px,
                        );
                        encode_kitty_tui_payload(&png_bytes, axis)
                    }
                    TerminalImageProtocol::ITerm2 => {
                        let b64 = B64_STANDARD.encode(&png_bytes);
                        format!(
                            "\x1b]1337;File=inline=1;width={draw_cols};height={draw_rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07"
                        )
                    }
                    TerminalImageProtocol::Sixel | TerminalImageProtocol::Halfblock => {
                        return None;
                    }
                };
                Some((payload, draw_cols, draw_rows))
            }
        }
    }
}

/// Tracks an active terminal pixel graphic placement (Kitty / iTerm2 / Sixel) painted onto the screen
/// so frame transitions can deterministically erase stale pixels before Ratatui redraws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
    pub cols: u16,
    pub rows: u16,
    pub payload_len: usize,
}

/// Returns the Kitty graphics delete sequence for the fixed TUI preview placement (`q=2` suppresses terminal responses).
pub fn kitty_delete_preview_sequence() -> &'static str {
    "\x1b_Ga=d,d=i,i=4242,q=2\x1b\\"
}

/// Deletes all visible Kitty graphics placements and frees unused image data (`d=A`, `q=2` quiet).
pub fn kitty_delete_all_sequence() -> &'static str {
    "\x1b_Ga=d,d=A,q=2\x1b\\"
}

/// Builds an ANSI sequence (`\x1b[0m` + row cursor moves + space runs) that overwrites `rect`
/// (expanded by +2 cols / +2 rows and clamped to `term_cols x term_rows`) so Sixel and iTerm2
/// cell-bound pixel tiles are deterministically erased before Ratatui redraws.
pub fn erase_pixel_rect_sequence(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    term_cols: u16,
    term_rows: u16,
) -> String {
    if width == 0 || height == 0 || x >= term_cols || y >= term_rows {
        return String::new();
    }
    let x_end = x.saturating_add(width).saturating_add(2).min(term_cols);
    let y_end = y.saturating_add(height).saturating_add(2).min(term_rows);
    if x >= x_end || y >= y_end {
        return String::new();
    }
    let span_w = (x_end - x) as usize;
    let rows = (y_end - y) as usize;
    let spaces = " ".repeat(span_w);
    let mut out = String::with_capacity(4 + rows * (12 + span_w));
    out.push_str("\x1b[0m");
    for row in y..y_end {
        let _ = write!(out, "\x1b[{};{}H{spaces}", row + 1, x + 1);
    }
    out
}

/// Floors the maximum vertical pixel budget for Sixel encoding to a multiple of 6
/// (`<= rows * cell_px_h`) so the final 6-pixel band never overhangs into the row below `rect`.
pub fn compute_sixel_safe_pixel_height(rows: u16, cell_px_h: u16) -> u32 {
    let budget = (rows as u32).saturating_mul(cell_px_h as u32);
    (budget / 6) * 6
}

/// Resolves terminal graphics protocol and cell pixel dimensions without writing queries to stdin/stdout.
pub fn detect_graphics_caps(pref: ImageProtocolPreference) -> TerminalGraphicsCaps {
    let protocol = resolve_protocol_with(pref, |k| std::env::var(k).ok());
    let (cell_px, cell_px_exact) = detect_cell_pixel_size();
    TerminalGraphicsCaps {
        protocol,
        cell_px,
        cell_px_exact,
    }
}

/// Computes rounded cell pixel dimensions `(cell_w, cell_h)` from window pixel dimensions and cell grid.
pub fn compute_rounded_cell_px(
    win_w: u16,
    win_h: u16,
    cols: u16,
    rows: u16,
) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 || win_w < cols || win_h < rows {
        return None;
    }
    let cw = ((win_w as u32 + cols as u32 / 2) / cols as u32).clamp(4, 64) as u16;
    let ch = ((win_h as u32 + rows as u32 / 2) / rows as u32).clamp(8, 128) as u16;
    Some((cw, ch))
}

fn apply_probe_reply(
    caps: &mut TerminalGraphicsCaps,
    reply: &str,
    need_cell_px: bool,
    need_proto: bool,
    term_size: Option<(u16, u16)>,
) -> bool {
    let mut cell_px_changed = false;
    if need_cell_px {
        let measured = parse_csi_cell_size_reply(reply).or_else(|| {
            let (win_w, win_h) = parse_csi_win_size_reply(reply)?;
            let (cols, rows) = term_size?;
            compute_rounded_cell_px(win_w, win_h, cols, rows)
        });
        if let Some(cell_px) = measured {
            if caps.cell_px != cell_px || !caps.cell_px_exact {
                caps.cell_px = cell_px;
                caps.cell_px_exact = true;
                cell_px_changed = true;
            }
        }
    }

    if need_proto {
        if parse_kitty_query_ok(reply) {
            caps.protocol = TerminalImageProtocol::Kitty;
        } else if parse_da1_has_sixel(reply) {
            caps.protocol = TerminalImageProtocol::Sixel;
        }
    }

    cell_px_changed
}

/// Actively probes terminal graphics capabilities and exact cell pixel size once on TUI startup
/// while the terminal is in raw mode (non-blocking, bounded to `PROBE_TIMEOUT`).
pub fn probe_terminal_graphics_caps(pref: ImageProtocolPreference) -> TerminalGraphicsCaps {
    let mut caps = detect_graphics_caps(pref);

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return caps;
    }

    let get_env = |k: &str| std::env::var(k).ok();
    let has_explicit_env_protocol = first_non_empty_env(&get_env, ENV_PROTOCOL_KEYS)
        .and_then(|raw| ImageProtocolPreference::parse_str(&raw))
        .is_some_and(|p| p != ImageProtocolPreference::Auto);
    let in_multiplexer =
        non_empty_env(&get_env, "TMUX").is_some() || non_empty_env(&get_env, "STY").is_some();

    let need_cell_px = !caps.cell_px_exact;
    let need_proto = pref == ImageProtocolPreference::Auto
        && !caps.protocol.is_pixel()
        && !has_explicit_env_protocol
        && !in_multiplexer;

    if !need_cell_px && !need_proto {
        return caps;
    }

    let mut query = String::with_capacity(96);
    if need_cell_px {
        query.push_str("\x1b[16t\x1b[14t");
    }
    if need_proto {
        query.push_str("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    }
    // DA1 serves as both a Sixel capability probe (`;4`) and a completion sentinel.
    query.push_str("\x1b[c");

    let reply = write_and_read_terminal_probe(&query, PROBE_TIMEOUT);
    if reply.is_empty() {
        return caps;
    }

    let term_size = crossterm::terminal::size().ok();
    apply_probe_reply(&mut caps, &reply, need_cell_px, need_proto, term_size);
    caps
}

/// Refreshes exact cell pixel size on terminal resize (HiDPI monitor move or font zoom).
/// On Windows, callers running an active `EventStream` must drop it before calling this and
/// re-apply mouse capture + recreate `EventStream` afterward.
pub fn refresh_cell_pixel_size(caps: &mut TerminalGraphicsCaps) -> bool {
    let get_env = |k: &str| std::env::var(k).ok();
    if let Some(raw) = first_non_empty_env(&get_env, ENV_CELL_PX_KEYS) {
        if let Some(parsed) = parse_cell_pixel_size(&raw) {
            let changed = caps.cell_px != parsed || !caps.cell_px_exact;
            caps.cell_px = parsed;
            caps.cell_px_exact = true;
            return changed;
        }
    }

    #[cfg(unix)]
    {
        if let Ok(ws) = crossterm::terminal::window_size() {
            if let Some(cell_px) = compute_rounded_cell_px(ws.width, ws.height, ws.columns, ws.rows)
            {
                let changed = caps.cell_px != cell_px || !caps.cell_px_exact;
                caps.cell_px = cell_px;
                caps.cell_px_exact = true;
                return changed;
            }
        }
        false
    }

    #[cfg(windows)]
    {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return false;
        }
        let reply = write_and_read_terminal_probe("\x1b[16t\x1b[14t\x1b[c", PROBE_TIMEOUT);
        if reply.is_empty() {
            return false;
        }
        let term_size = crossterm::terminal::size().ok();
        apply_probe_reply(caps, &reply, true, false, term_size)
    }

    #[cfg(not(any(windows, unix)))]
    {
        let _ = caps;
        false
    }
}

/// Parses an xterm `CSI 16 t` cell pixel size reply (`\x1b[6;<height>;<width>t`) into `(cell_w, cell_h)`.
pub fn parse_csi_cell_size_reply(reply: &str) -> Option<(u16, u16)> {
    let (h, w) = parse_csi_two_param_t(reply, "\x1b[6;")?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w.clamp(4, 64), h.clamp(8, 128)))
}

/// Parses an xterm `CSI 14 t` window pixel size reply (`\x1b[4;<height>;<width>t`) into `(win_w, win_h)`.
pub fn parse_csi_win_size_reply(reply: &str) -> Option<(u16, u16)> {
    let (h, w) = parse_csi_two_param_t(reply, "\x1b[4;")?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

fn parse_csi_two_param_t(reply: &str, prefix: &str) -> Option<(u16, u16)> {
    let mut rest = reply;
    while let Some(idx) = rest.find(prefix) {
        let after = &rest[idx + prefix.len()..];
        if let Some(end) = after.find('t') {
            let body = &after[..end];
            if let Some((first, second)) = body.split_once(';') {
                if let (Ok(a), Ok(b)) = (first.parse::<u16>(), second.parse::<u16>()) {
                    if a > 0 && b > 0 {
                        return Some((a, b));
                    }
                }
            }
        }
        rest = after;
    }
    None
}

/// Returns `true` when a Kitty graphics query (`a=q`) reply reports `OK`.
pub fn parse_kitty_query_ok(reply: &str) -> bool {
    let mut rest = reply;
    while let Some(idx) = rest.find("\x1b_G") {
        let after = &rest[idx + 3..];
        let end_st = after.find("\x1b\\");
        let end_bel = after.find('\x07');
        let end = match (end_st, end_bel) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        let body = &after[..end];
        if let Some((params, status)) = body.split_once(';') {
            if status.trim() == "OK" {
                let id_ok = params
                    .split(',')
                    .filter_map(|kv| kv.strip_prefix("i="))
                    .next()
                    .is_none_or(|id| id == "31");
                if id_ok {
                    return true;
                }
            }
        }
        rest = &after[end..];
    }
    false
}

/// Returns `true` when a Primary Device Attributes (`DA1`, `\x1b[?...c`) reply advertises Sixel (`;4`).
pub fn parse_da1_has_sixel(reply: &str) -> bool {
    let mut rest = reply;
    while let Some(idx) = rest.find("\x1b[?") {
        let after = &rest[idx + 3..];
        if let Some(end) = after.find('c') {
            let body = &after[..end];
            if !body.is_empty()
                && body.bytes().all(|b| b.is_ascii_digit() || b == b';')
                && body.split(';').skip(1).any(|attr| attr == "4")
            {
                return true;
            }
        }
        rest = after;
    }
    false
}

fn has_complete_da1_reply(reply: &str) -> bool {
    let mut rest = reply;
    while let Some(idx) = rest.find("\x1b[?") {
        let after = &rest[idx + 3..];
        if let Some(end) = after.find('c') {
            let body = &after[..end];
            if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit() || b == b';') {
                return true;
            }
        }
        rest = after;
    }
    false
}

#[cfg(any(windows, test))]
const WIN_ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
#[cfg(any(windows, test))]
const WIN_ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
#[cfg(any(windows, test))]
const WIN_ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;

/// Restores Windows console input mode while always setting `ENABLE_EXTENDED_FLAGS` (`0x0080`)
/// and clearing `ENABLE_QUICK_EDIT_MODE` (`0x0040`) so mouse capture is never clobbered by QuickEdit.
#[cfg(any(windows, test))]
fn restore_windows_console_mode_flags(prev_mode: u32) -> u32 {
    (prev_mode & !WIN_ENABLE_QUICK_EDIT_MODE) | WIN_ENABLE_EXTENDED_FLAGS
}

/// Enables `ENABLE_VIRTUAL_TERMINAL_INPUT` via an RAII guard BEFORE writing the probe query to
/// stdout and flushing, then reads the reply and restores the prior console mode on exit.
#[cfg(any(windows, test))]
fn run_probe_with_mode_guard<S, W, R>(
    prev_mode: u32,
    mut set_mode: S,
    writer: &mut W,
    query: &str,
    read_reply: R,
) -> String
where
    S: FnMut(u32),
    W: std::io::Write,
    R: FnOnce() -> String,
{
    struct ModeRestore<'a, F: FnMut(u32)> {
        restore_mode: u32,
        set_mode: &'a mut F,
    }
    impl<F: FnMut(u32)> Drop for ModeRestore<'_, F> {
        fn drop(&mut self) {
            (self.set_mode)(self.restore_mode);
        }
    }

    let restore_mode = restore_windows_console_mode_flags(prev_mode);
    let probe_mode = restore_mode | WIN_ENABLE_VIRTUAL_TERMINAL_INPUT;
    set_mode(probe_mode);
    let _restore = ModeRestore {
        restore_mode,
        set_mode: &mut set_mode,
    };

    if writer.write_all(query.as_bytes()).is_err() || writer.flush().is_err() {
        return String::new();
    }

    read_reply()
}

#[cfg(windows)]
fn write_and_read_terminal_probe(query: &str, timeout: Duration) -> String {
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;
    const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
    const WAIT_OBJECT_0: u32 = 0;
    const KEY_EVENT: u16 = 1;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct KeyEventRecord {
        b_key_down: i32,
        w_repeat_count: u16,
        w_virtual_key_code: u16,
        w_virtual_scan_code: u16,
        unicode_char: u16,
        dw_control_key_state: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    union EventUnion {
        key: KeyEventRecord,
        raw: [u32; 4],
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct InputRecord {
        event_type: u16,
        event: EventUnion,
    }

    const _: () = assert!(std::mem::size_of::<InputRecord>() == 20);

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(
            lp_file_name: *const u16,
            dw_desired_access: u32,
            dw_share_mode: u32,
            lp_security_attributes: *mut std::ffi::c_void,
            dw_creation_disposition: u32,
            dw_flags_and_attributes: u32,
            h_template_file: isize,
        ) -> isize;
        fn CloseHandle(h_object: isize) -> i32;
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn GetConsoleMode(h_console_handle: isize, lp_mode: *mut u32) -> i32;
        fn SetConsoleMode(h_console_handle: isize, dw_mode: u32) -> i32;
        fn WaitForSingleObject(h_handle: isize, dw_milliseconds: u32) -> u32;
        fn GetNumberOfConsoleInputEvents(h_console_input: isize, lpc_number_of_events: *mut u32)
            -> i32;
        fn PeekConsoleInputW(
            h_console_input: isize,
            lp_buffer: *mut InputRecord,
            n_length: u32,
            lp_number_of_events_read: *mut u32,
        ) -> i32;
        fn ReadConsoleInputW(
            h_console_input: isize,
            lp_buffer: *mut InputRecord,
            n_length: u32,
            lp_number_of_events_read: *mut u32,
        ) -> i32;
    }

    struct OwnedHandle(Option<isize>);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if let Some(h) = self.0 {
                unsafe {
                    CloseHandle(h);
                }
            }
        }
    }

    const CONIN_WIDE: [u16; 7] = [
        b'C' as u16,
        b'O' as u16,
        b'N' as u16,
        b'I' as u16,
        b'N' as u16,
        b'$' as u16,
        0,
    ];
    let conin = unsafe {
        CreateFileW(
            CONIN_WIDE.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            0,
        )
    };
    let (handle, _owned_handle) = if conin != 0 && conin != INVALID_HANDLE_VALUE {
        (conin, OwnedHandle(Some(conin)))
    } else {
        let std_in = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        (std_in, OwnedHandle(None))
    };
    if handle == 0 || handle == INVALID_HANDLE_VALUE {
        return String::new();
    }

    let mut prev_mode = 0u32;
    if unsafe { GetConsoleMode(handle, &mut prev_mode) } == 0 {
        return String::new();
    }

    let mut stdout = std::io::stdout();
    run_probe_with_mode_guard(
        prev_mode,
        |mode| unsafe {
            let _ = SetConsoleMode(handle, mode);
        },
        &mut stdout,
        query,
        || {
            let deadline = Instant::now() + timeout;
            let mut reply = String::with_capacity(128);
            let mut records = [InputRecord {
                event_type: 0,
                event: EventUnion { raw: [0; 4] },
            }; 64];

            while reply.len() < 1024 {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                let rem_ms = (deadline - now).as_millis().clamp(1, u32::MAX as u128) as u32;
                let wait_res = unsafe { WaitForSingleObject(handle, rem_ms) };
                if wait_res != WAIT_OBJECT_0 {
                    break;
                }
                let mut avail = 0u32;
                if unsafe { GetNumberOfConsoleInputEvents(handle, &mut avail) } == 0 || avail == 0 {
                    break;
                }
                let to_read = (avail as usize).min(records.len()) as u32;
                let mut read_count = 0u32;
                if unsafe {
                    ReadConsoleInputW(handle, records.as_mut_ptr(), to_read, &mut read_count)
                } == 0
                    || read_count == 0
                {
                    break;
                }
                for rec in &records[..read_count as usize] {
                    if rec.event_type == KEY_EVENT {
                        let key = unsafe { rec.event.key };
                        if key.b_key_down != 0 && key.unicode_char > 0 && key.unicode_char <= 0x7f {
                            let ch = key.unicode_char as u8 as char;
                            let reps = key.w_repeat_count.clamp(1, 16);
                            for _ in 0..reps {
                                reply.push(ch);
                            }
                        }
                    }
                }
                if has_complete_da1_reply(&reply) {
                    break;
                }
            }

            // Non-blocking drain of any trailing KEY_EVENT bytes from the probe burst so nothing leaks into EventStream.
            for _ in 0..8 {
                if unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0 {
                    break;
                }
                let mut avail = 0u32;
                if unsafe { GetNumberOfConsoleInputEvents(handle, &mut avail) } == 0 || avail == 0 {
                    break;
                }
                let to_peek = (avail as usize).min(records.len()) as u32;
                let mut peeked = 0u32;
                if unsafe { PeekConsoleInputW(handle, records.as_mut_ptr(), to_peek, &mut peeked) }
                    == 0
                    || peeked == 0
                {
                    break;
                }
                let key_prefix = records[..peeked as usize]
                    .iter()
                    .take_while(|rec| rec.event_type == KEY_EVENT)
                    .count() as u32;
                if key_prefix == 0 {
                    break;
                }
                let mut read_count = 0u32;
                if unsafe {
                    ReadConsoleInputW(handle, records.as_mut_ptr(), key_prefix, &mut read_count)
                } == 0
                    || read_count == 0
                {
                    break;
                }
                for rec in &records[..read_count as usize] {
                    if rec.event_type == KEY_EVENT {
                        let key = unsafe { rec.event.key };
                        if key.b_key_down != 0
                            && key.unicode_char > 0
                            && key.unicode_char <= 0x7f
                            && reply.len() < 1024
                        {
                            let ch = key.unicode_char as u8 as char;
                            let reps = key.w_repeat_count.clamp(1, 16);
                            for _ in 0..reps {
                                reply.push(ch);
                            }
                        }
                    }
                }
            }

            reply
        },
    )
}

#[cfg(unix)]
fn write_and_read_terminal_probe(query: &str, timeout: Duration) -> String {
    use std::io::{Read as _, Write as _};
    use std::os::unix::io::AsRawFd;

    const POLLIN: i16 = 0x0001;

    #[cfg(target_os = "macos")]
    type Nfds = u32;
    #[cfg(not(target_os = "macos"))]
    type Nfds = usize;

    #[repr(C)]
    struct PollFd {
        fd: i32,
        events: i16,
        revents: i16,
    }

    extern "C" {
        fn poll(fds: *mut PollFd, nfds: Nfds, timeout: i32) -> i32;
    }

    {
        let mut stdout = std::io::stdout();
        if stdout.write_all(query.as_bytes()).is_err() || stdout.flush().is_err() {
            return String::new();
        }
    }

    let mut stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 256];
    let mut reply = String::with_capacity(128);

    while reply.len() < 1024 {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let rem_ms = (deadline - now).as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut pfd = PollFd {
            fd,
            events: POLLIN,
            revents: 0,
        };
        let ready = unsafe { poll(&mut pfd, 1 as Nfds, rem_ms) };
        if ready <= 0 || (pfd.revents & POLLIN) == 0 {
            break;
        }
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b <= 0x7f {
                        reply.push(b as char);
                    }
                }
                if has_complete_da1_reply(&reply) {
                    break;
                }
            }
        }
    }

    reply
}

#[cfg(not(any(windows, unix)))]
fn write_and_read_terminal_probe(_query: &str, _timeout: Duration) -> String {
    String::new()
}

/// Returns `true` when running inside a StackBite managed seat/PTY where inherited outer
/// terminal env vars (such as `WT_SESSION`) must never trigger static pixel misclassification.
fn is_stackbite_env<F>(get_env: &F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    non_empty_env(get_env, "STACKBITE_ROOM_ID").is_some()
        || non_empty_env(get_env, "STACKBITE_HOME").is_some()
        || non_empty_env(get_env, "STACKBITE_PTY_BACKEND").is_some()
}

/// Pure protocol resolver taking an environment variable map (for deterministic testing and diagnostics).
pub fn resolve_protocol_from_env_map(
    pref: ImageProtocolPreference,
    env: &HashMap<&str, &str>,
) -> TerminalImageProtocol {
    resolve_protocol_with(pref, |k| env.get(k).map(|v| (*v).to_string()))
}

fn resolve_protocol_with<F>(pref: ImageProtocolPreference, get_env: F) -> TerminalImageProtocol
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(raw) = first_non_empty_env(&get_env, ENV_PROTOCOL_KEYS) {
        if let Some(parsed) = ImageProtocolPreference::parse_str(&raw) {
            if parsed != ImageProtocolPreference::Auto {
                return protocol_from_pref(parsed);
            }
        }
    }

    if pref != ImageProtocolPreference::Auto {
        return protocol_from_pref(pref);
    }

    // Multiplexer guard: avoid raw pixel escape sequences inside tmux/screen without DCS passthrough.
    if non_empty_env(&get_env, "TMUX").is_some() || non_empty_env(&get_env, "STY").is_some() {
        return TerminalImageProtocol::Halfblock;
    }

    // StackBite PTY guard: never trust inherited outer terminal variables (e.g. WT_SESSION);
    // TUI will actively probe Kitty a=q + DA1 ;4 and only accept measured replies.
    if is_stackbite_env(&get_env) {
        return TerminalImageProtocol::Halfblock;
    }

    let term_program = lower_env(&get_env, "TERM_PROGRAM");
    let term = lower_env(&get_env, "TERM");

    // Kitty Graphics Protocol (Kitty, Ghostty, and WezTerm — checked first so they win over iTerm2/Sixel).
    if non_empty_env(&get_env, "KITTY_WINDOW_ID").is_some()
        || non_empty_env(&get_env, "GHOSTTY_RESOURCES_DIR").is_some()
        || non_empty_env(&get_env, "WEZTERM_EXECUTABLE").is_some()
        || non_empty_env(&get_env, "WEZTERM_PANE").is_some()
        || matches!(term_program.as_str(), "kitty" | "ghostty" | "wezterm")
        || term.contains("xterm-kitty")
        || term.contains("ghostty")
    {
        return TerminalImageProtocol::Kitty;
    }

    // iTerm2 Inline Images (`OSC 1337`)
    if matches!(
        term_program.as_str(),
        "iterm.app" | "rio" | "mintty" | "hyper"
    ) {
        return TerminalImageProtocol::ITerm2;
    }

    // Sixel (`DCS q`) for known native Sixel terminals.
    // Note: `WT_SESSION` is intentionally NOT forced to Sixel here; `Auto` always verifies
    // Sixel via DA1 `;4` (and Kitty via `a=q`) during `probe_terminal_graphics_caps`.
    if matches!(
        term.as_str(),
        "foot" | "foot-extra" | "mlterm" | "yaft-256color"
    ) || matches!(term_program.as_str(), "foot" | "contour")
    {
        return TerminalImageProtocol::Sixel;
    }

    TerminalImageProtocol::Halfblock
}

fn protocol_from_pref(pref: ImageProtocolPreference) -> TerminalImageProtocol {
    match pref {
        ImageProtocolPreference::Auto | ImageProtocolPreference::Halfblock => {
            TerminalImageProtocol::Halfblock
        }
        ImageProtocolPreference::Kitty => TerminalImageProtocol::Kitty,
        ImageProtocolPreference::Iterm2 => TerminalImageProtocol::ITerm2,
        ImageProtocolPreference::Sixel => TerminalImageProtocol::Sixel,
    }
}

fn first_non_empty_env<F>(get_env: &F, keys: &[&str]) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    keys.iter().find_map(|k| non_empty_env(get_env, k))
}

fn non_empty_env<F>(get_env: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key).and_then(|v| {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn lower_env<F>(get_env: &F, key: &str) -> String
where
    F: Fn(&str) -> Option<String>,
{
    get_env(key)
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default()
}

fn detect_cell_pixel_size() -> ((u16, u16), bool) {
    let get_env = |k: &str| std::env::var(k).ok();
    if let Some(raw) = first_non_empty_env(&get_env, ENV_CELL_PX_KEYS) {
        if let Some(parsed) = parse_cell_pixel_size(&raw) {
            return (parsed, true);
        }
    }
    if let Ok(ws) = crossterm::terminal::window_size() {
        if let Some(cell_px) = compute_rounded_cell_px(ws.width, ws.height, ws.columns, ws.rows) {
            return (cell_px, true);
        }
    }
    (default_cell_pixel_size(), false)
}

fn parse_cell_pixel_size(raw: &str) -> Option<(u16, u16)> {
    let (w_str, h_str) = raw.trim().split_once(['x', 'X'])?;
    let w = w_str.trim().parse::<u16>().ok()?;
    let h = h_str.trim().parse::<u16>().ok()?;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w.clamp(4, 64), h.clamp(8, 128)))
}

/// Returns true if raw bytes look like an HTML document (e.g., SSO login page or error page).
pub fn looks_like_html_bytes(bytes: &[u8]) -> bool {
    let trimmed = strip_bom_and_ws(bytes);
    if trimmed.is_empty() || trimmed[0] != b'<' {
        return false;
    }
    let prefix = ascii_lower_prefix(trimmed, 512);
    if prefix.starts_with("<!doctype html")
        || prefix.starts_with("<html")
        || prefix.starts_with("<head")
        || prefix.starts_with("<body")
        || prefix.starts_with("<meta")
        || prefix.starts_with("<title")
        || prefix.starts_with("<script")
        || prefix.starts_with("<div")
        || prefix.starts_with("<p>")
        || prefix.starts_with("<p ")
    {
        return true;
    }
    prefix.starts_with("<!--")
        && (prefix.contains("<!doctype html")
            || prefix.contains("<html")
            || prefix.contains("<head")
            || prefix.contains("<body"))
}

/// Returns true if raw bytes look like an SVG XML document (vector graphics, not raster).
pub fn looks_like_svg_bytes(bytes: &[u8]) -> bool {
    let trimmed = strip_bom_and_ws(bytes);
    if trimmed.is_empty() || trimmed[0] != b'<' {
        return false;
    }
    let prefix = ascii_lower_prefix(trimmed, 512);
    if let Some(rest) = prefix.strip_prefix("<svg") {
        if rest.is_empty()
            || rest.starts_with(|c: char| c.is_ascii_whitespace() || c == '>' || c == '/')
        {
            return true;
        }
    }
    if prefix.starts_with("<!doctype svg") {
        return true;
    }
    (prefix.starts_with("<?xml") || prefix.starts_with("<!--"))
        && (prefix.contains("<svg ") || prefix.contains("<svg>") || prefix.contains("<svg\n") || prefix.contains("<svg\t"))
}

fn strip_bom_and_ws(bytes: &[u8]) -> &[u8] {
    let mut slice = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    while let Some((&first, rest)) = slice.split_first() {
        if first.is_ascii_whitespace() {
            slice = rest;
        } else {
            break;
        }
    }
    slice
}

fn ascii_lower_prefix(bytes: &[u8], max_len: usize) -> String {
    let take = bytes.len().min(max_len);
    bytes[..take]
        .iter()
        .map(|&b| (b.to_ascii_lowercase()) as char)
        .collect()
}

fn validate_raster_image_bytes(bytes: &[u8]) -> Result<()> {
    if looks_like_html_bytes(bytes) {
        bail!("Received HTML content instead of image bytes (likely an authentication redirect or error page)");
    }
    Ok(())
}

/// Returns true if a MIME type or filename looks like a supported previewable image
/// (`png`, `jpeg`, `jpg`, `gif`, `webp`, or `svg`). Explicitly excludes HTML (`text/html`).
pub fn is_image_mime_or_name(content_type: &str, file_name: Option<&str>) -> bool {
    let ct = content_type.trim().to_ascii_lowercase();
    let ct_base = ct.split(';').next().unwrap_or("").trim();
    if matches!(ct_base, "text/html" | "application/xhtml+xml") {
        return false;
    }
    if matches!(
        ct_base,
        "image/png"
            | "image/jpeg"
            | "image/jpg"
            | "image/gif"
            | "image/webp"
            | "image/svg+xml"
            | "hostedcontent"
    ) {
        return true;
    }
    if let Some(name) = file_name {
        let lower = name.trim().to_ascii_lowercase();
        if lower.ends_with(".png")
            || lower.ends_with(".jpg")
            || lower.ends_with(".jpeg")
            || lower.ends_with(".gif")
            || lower.ends_with(".webp")
            || lower.ends_with(".svg")
        {
            return true;
        }
    }
    false
}

/// Converts a Ratatui `Color` token into an RGB triple for alpha blending, falling back to `fallback` for `Color::Reset`.
pub fn color_to_rgb(color: Color, fallback: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (204, 204, 204),
        Color::DarkGray => (118, 118, 118),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (255, 255, 255),
        Color::Indexed(idx) => ansi256_to_rgb(idx),
        Color::Reset => fallback,
    }
}

fn ansi256_to_rgb(idx: u8) -> (u8, u8, u8) {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (204, 204, 204),
        (118, 118, 118),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];
    if idx < 16 {
        return ANSI16[idx as usize];
    }
    if idx < 232 {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        let v = idx - 16;
        let r = LEVELS[(v / 36) as usize];
        let g = LEVELS[((v % 36) / 6) as usize];
        let b = LEVELS[(v % 6) as usize];
        return (r, g, b);
    }
    let gray = 8 + (idx - 232) * 10;
    (gray, gray, gray)
}

fn svg_raster_bounds(fit: FitBox, cell_px: (u16, u16)) -> (u32, u32) {
    let max_dim = bitefoundry_html::graphics::MAX_RASTER_DIM_PX;
    let zoom = fit.zoom_pct.clamp(25, 300) as u32;
    let cw = cell_px.0.max(1) as u32;
    let ch = cell_px.1.max(1) as u32;
    let max_px_w = ((fit.view_cols as u32).saturating_mul(cw).saturating_mul(zoom) / 100)
        .clamp(256, max_dim);
    let max_px_h = if fit.scrollable {
        max_dim
    } else {
        ((fit.view_rows as u32).saturating_mul(ch).saturating_mul(zoom) / 100).clamp(256, max_dim)
    };
    (max_px_w, max_px_h)
}

/// Decodes an image buffer and renders aspect-ratio-preserving Truecolor Unicode Half-Blocks (`▀`)
/// using actual measured `cell_px` so non-1:2 fonts keep accurate physical proportions.
pub fn decode_and_render_halfblocks_with_cell_px(
    bytes: &[u8],
    fit: FitBox,
    bg_rgb: (u8, u8, u8),
    cell_px: (u16, u16),
) -> Result<RenderedTerminalImage> {
    validate_raster_image_bytes(bytes)?;
    let rasterized_svg;
    let (decode_bytes, format_label): (&[u8], String) = if looks_like_svg_bytes(bytes) {
        let (max_px_w, max_px_h) = svg_raster_bounds(fit, cell_px);
        rasterized_svg =
            bitefoundry_html::graphics::rasterize_svg_to_png(bytes, max_px_w, max_px_h, None)
                .context("Failed to rasterize SVG")?;
        (&rasterized_svg.png_bytes, "SVG".to_string())
    } else {
        let guessed = image::guess_format(bytes).ok();
        (bytes, format_label_for(guessed))
    };
    let dyn_img = image::load_from_memory(decode_bytes).context("Failed to decode image")?;
    let rgba = dyn_img.into_rgba8();
    let (width_px, height_px) = rgba.dimensions();
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_w, target_h) = compute_halfblock_dims(width_px, height_px, fit, cell_px);

    let resized = image::imageops::resize(&rgba, target_w, target_h, FilterType::Lanczos3);
    let mut lines = Vec::with_capacity((target_h / 2) as usize);

    type Rgb = (u8, u8, u8);
    for y in (0..target_h).step_by(2) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut current_pair: Option<(Rgb, Rgb)> = None;
        let mut run = String::new();

        for x in 0..target_w {
            let top = blend_over_bg(*resized.get_pixel(x, y), bg_rgb);
            let bot = blend_over_bg(*resized.get_pixel(x, y + 1), bg_rgb);
            let pair = (top, bot);

            if current_pair == Some(pair) {
                run.push('▀');
            } else {
                if let Some((prev_top, prev_bot)) = current_pair {
                    spans.push(Span::styled(
                        std::mem::take(&mut run),
                        Style::default()
                            .fg(Color::Rgb(prev_top.0, prev_top.1, prev_top.2))
                            .bg(Color::Rgb(prev_bot.0, prev_bot.1, prev_bot.2)),
                    ));
                }
                current_pair = Some(pair);
                run.push('▀');
            }
        }

        if let Some((prev_top, prev_bot)) = current_pair {
            spans.push(Span::styled(
                run,
                Style::default()
                    .fg(Color::Rgb(prev_top.0, prev_top.1, prev_top.2))
                    .bg(Color::Rgb(prev_bot.0, prev_bot.1, prev_bot.2)),
            ));
        }

        lines.push(Line::from(spans));
    }

    Ok(RenderedTerminalImage {
        width_px,
        height_px,
        cols: target_w as u16,
        rows: (target_h / 2) as u16,
        format_label,
        protocol: TerminalImageProtocol::Halfblock,
        lines,
        pixel_payload: None,
        scroll_backing: None,
    })
}

/// Convenience wrapper rendering Half-Blocks with a fixed box and platform default `cell_px`.
pub fn decode_and_render_halfblocks(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    bg_rgb: (u8, u8, u8),
) -> Result<RenderedTerminalImage> {
    decode_and_render_halfblocks_with_cell_px(
        bytes,
        FitBox::fixed(max_cols, max_rows),
        bg_rgb,
        default_cell_pixel_size(),
    )
}

/// Decodes and renders an image using the requested terminal graphics protocol (`Kitty`, `ITerm2`, `Sixel`, or `Halfblock`).
pub fn decode_and_render_for_protocol(
    bytes: &[u8],
    fit: FitBox,
    bg_rgb: (u8, u8, u8),
    caps: TerminalGraphicsCaps,
) -> Result<RenderedTerminalImage> {
    if caps.protocol == TerminalImageProtocol::Halfblock {
        return decode_and_render_halfblocks_with_cell_px(bytes, fit, bg_rgb, caps.cell_px);
    }

    validate_raster_image_bytes(bytes)?;
    let rasterized_svg;
    let (decode_bytes, guessed, format_label): (&[u8], Option<ImageFormat>, String) =
        if looks_like_svg_bytes(bytes) {
            let (max_px_w, max_px_h) = svg_raster_bounds(fit, caps.cell_px);
            rasterized_svg =
                bitefoundry_html::graphics::rasterize_svg_to_png(bytes, max_px_w, max_px_h, None)
                    .context("Failed to rasterize SVG")?;
            (
                &rasterized_svg.png_bytes,
                Some(ImageFormat::Png),
                "SVG".to_string(),
            )
        } else {
            let guessed = image::guess_format(bytes).ok();
            (bytes, guessed, format_label_for(guessed))
        };
    let dyn_img = image::load_from_memory(decode_bytes).context("Failed to decode image")?;
    let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_px_w, target_px_h, occ_cols, occ_rows) =
        compute_pixel_fit_for_protocol(width_px, height_px, fit, caps.cell_px, caps.protocol);

    let (payload, scroll_backing) = match caps.protocol {
        TerminalImageProtocol::Kitty => {
            let source_img = if width_px <= MAX_RENDER_PX_W && height_px <= MAX_RENDER_PX_H {
                dyn_img
            } else {
                fit_within_max_bounds(&dyn_img, MAX_RENDER_PX_W, MAX_RENDER_PX_H)
            };
            let (src_w, src_h) = (source_img.width(), source_img.height());
            let png_owned;
            let png_bytes: &[u8] = if matches!(guessed, Some(ImageFormat::Png))
                && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
            {
                decode_bytes
            } else {
                png_owned = encode_png(&source_img)?;
                &png_owned
            };
            let axis = choose_kitty_fit_axis(src_w, src_h, occ_cols, occ_rows, caps.cell_px);
            let backing = Box::new(PixelScrollBacking::Source {
                image: Arc::new(source_img),
                target_px_w,
                target_px_h,
                cell_px: caps.cell_px,
            });
            (encode_kitty_tui_payload(png_bytes, axis), Some(backing))
        }
        TerminalImageProtocol::ITerm2 => {
            let source_img = if width_px <= MAX_RENDER_PX_W && height_px <= MAX_RENDER_PX_H {
                dyn_img
            } else {
                fit_within_max_bounds(&dyn_img, MAX_RENDER_PX_W, MAX_RENDER_PX_H)
            };
            let encoded_owned;
            let raw_bytes: &[u8] = if matches!(
                guessed,
                Some(
                    ImageFormat::Png
                        | ImageFormat::Jpeg
                        | ImageFormat::Gif
                        | ImageFormat::WebP
                )
            ) && decode_bytes.len() <= MAX_ITERM2_RAW_BYTES
            {
                decode_bytes
            } else {
                encoded_owned = encode_png(&source_img)?;
                &encoded_owned
            };
            let b64 = B64_STANDARD.encode(raw_bytes);
            let payload = format!(
                "\x1b]1337;File=inline=1;width={occ_cols};height={occ_rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07"
            );
            let backing = Box::new(PixelScrollBacking::Source {
                image: Arc::new(source_img),
                target_px_w,
                target_px_h,
                cell_px: caps.cell_px,
            });
            (payload, Some(backing))
        }
        TerminalImageProtocol::Sixel => {
            let rgba = dyn_img.to_rgba8();
            let resized = if width_px == target_px_w && height_px == target_px_h {
                rgba
            } else {
                image::imageops::resize(&rgba, target_px_w, target_px_h, FilterType::Lanczos3)
            };
            let mut flat_rgba = Vec::with_capacity((target_px_w * target_px_h * 4) as usize);
            for px in resized.pixels() {
                let (r, g, b) = blend_over_bg(*px, bg_rgb);
                flat_rgba.extend_from_slice(&[r, g, b, 255]);
            }
            let (palette, indexed) =
                quantize_rgba_to_256(&flat_rgba, target_px_w as usize, target_px_h as usize);
            let payload = encode_sixel_indexed_slice(
                &palette,
                &indexed,
                target_px_w as usize,
                0,
                0,
                target_px_w as usize,
                target_px_h as usize,
            );
            let backing = Box::new(PixelScrollBacking::SixelIndexed {
                width: target_px_w,
                height: target_px_h,
                cell_px: caps.cell_px,
                palette,
                indexed: Arc::new(indexed),
            });
            (payload, Some(backing))
        }
        TerminalImageProtocol::Halfblock => unreachable!("handled above"),
    };

    Ok(RenderedTerminalImage {
        width_px,
        height_px,
        cols: occ_cols,
        rows: occ_rows,
        format_label,
        protocol: caps.protocol,
        lines: Vec::new(),
        pixel_payload: Some(payload),
        scroll_backing,
    })
}

fn fit_within_max_bounds(img: &DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= max_w && h <= max_h {
        return img.clone();
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let nw = ((w as f64 * scale).round() as u32).clamp(1, max_w);
    let nh = ((h as f64 * scale).round() as u32).clamp(1, max_h);
    img.resize_exact(nw, nh, FilterType::Lanczos3)
}

fn choose_kitty_fit_axis(
    src_w: u32,
    src_h: u32,
    occ_cols: u16,
    occ_rows: u16,
    cell_px: (u16, u16),
) -> KittyFitAxis {
    let cell_w = cell_px.0.max(1) as u64;
    let cell_h = cell_px.1.max(1) as u64;
    let cols = occ_cols.max(1);
    let rows = occ_rows.max(1);
    let lhs = (src_w as u64) * (rows as u64) * cell_h;
    let rhs = (src_h as u64) * (cols as u64) * cell_w;
    if lhs > rhs || (lhs == rhs && src_w >= src_h) {
        KittyFitAxis::Cols(cols)
    } else {
        KittyFitAxis::Rows(rows)
    }
}

fn encode_kitty_tui_payload(png_bytes: &[u8], axis: KittyFitAxis) -> String {
    let b64 = B64_STANDARD.encode(png_bytes);
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).expect("base64 is valid ASCII"))
        .collect();
    let total = chunks.len();
    let mut out = String::with_capacity(b64.len() + total * 48);
    for (idx, chunk) in chunks.into_iter().enumerate() {
        let more = if idx + 1 < total { 1 } else { 0 };
        if idx == 0 {
            match axis {
                KittyFitAxis::Cols(c) => {
                    let _ = write!(
                        out,
                        "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={c},C=1,m={more};{chunk}\x1b\\"
                    );
                }
                KittyFitAxis::Rows(r) => {
                    let _ = write!(
                        out,
                        "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},r={r},C=1,m={more};{chunk}\x1b\\"
                    );
                }
            }
        } else {
            let _ = write!(out, "\x1b_Gq=2,m={more};{chunk}\x1b\\");
        }
    }
    out
}

fn is_tall_scrollable(
    width_px: u32,
    height_px: u32,
    vp_w: u64,
    vp_h: u64,
    scrollable: bool,
) -> bool {
    scrollable
        && height_px > width_px
        && (height_px as u64) * vp_w * 2 > (width_px as u64) * vp_h * 3
}

/// Computes `(target_cols_px, target_subpx_h)` for Half-Block rendering using actual `cell_px`
/// and explicit `FitBox`. `target_subpx_h` is always an even integer `>= 2`.
pub fn compute_halfblock_dims(
    width_px: u32,
    height_px: u32,
    fit: FitBox,
    cell_px: (u16, u16),
) -> (u32, u32) {
    const MAX_HALFBLOCK_COLS: f64 = 1024.0;
    const MAX_HALFBLOCK_SUBPX_H: f64 = 2048.0;

    let cell_w = cell_px.0.max(1) as f64;
    let cell_h = cell_px.1.max(1) as f64;
    let subpx_aspect = cell_h / (2.0 * cell_w);
    let eff_src_h = (height_px as f64 / subpx_aspect).max(1.0);

    let view_cols = fit.view_cols.max(1);
    let view_rows = fit.view_rows.max(1);
    let zoom_pct = fit.zoom_pct.clamp(25, 300);

    let vp_w = view_cols as f64;
    let vp_h = (view_rows as f64) * 2.0;
    let vp_phys_w = (view_cols as u64) * (cell_px.0.max(1) as u64);
    let vp_phys_h = (view_rows as u64) * (cell_px.1.max(1) as u64);
    let tall_scroll = is_tall_scrollable(width_px, height_px, vp_phys_w, vp_phys_h, fit.scrollable);

    let base_scale = if tall_scroll {
        (vp_w / width_px as f64).min(1.0)
    } else {
        (vp_w / width_px as f64).min(vp_h / eff_src_h).min(1.0)
    };
    let scale = base_scale * (zoom_pct as f64 / 100.0);

    let raw_w = (width_px as f64 * scale).max(1.0);
    let raw_h = (eff_src_h * scale).max(2.0);
    let cap_scale = (MAX_HALFBLOCK_COLS / raw_w)
        .min(MAX_HALFBLOCK_SUBPX_H / raw_h)
        .min(1.0);

    let mut target_w = (raw_w * cap_scale).round().max(1.0) as u32;
    let mut target_h = (raw_h * cap_scale).round().max(2.0) as u32;

    if !tall_scroll && zoom_pct <= 100 {
        target_w = target_w.min(view_cols as u32).max(1);
        target_h = target_h.min((view_rows as u32) * 2).max(2);
    }

    if !target_h.is_multiple_of(2) {
        let max_h = if !tall_scroll && zoom_pct <= 100 {
            (view_rows as u32) * 2
        } else {
            MAX_HALFBLOCK_SUBPX_H as u32
        };
        if target_h < max_h {
            target_h += 1;
        } else {
            target_h = target_h.saturating_sub(1).max(2);
        }
    }

    (target_w, target_h)
}

/// Computes `(target_px_w, target_px_h, occ_cols, occ_rows)` for pixel protocols from explicit `FitBox`.
pub fn compute_pixel_fit_for_protocol(
    width_px: u32,
    height_px: u32,
    fit: FitBox,
    cell_px: (u16, u16),
    protocol: TerminalImageProtocol,
) -> (u32, u32, u16, u16) {
    let cell_w = cell_px.0.max(1) as u32;
    let cell_h = cell_px.1.max(1) as u32;
    let view_cols = fit.view_cols.max(1);
    let view_rows = fit.view_rows.max(1);
    let zoom_pct = fit.zoom_pct.clamp(25, 300);

    let vp_w = (view_cols as u32).saturating_mul(cell_w).max(1);
    let raw_vp_h = (view_rows as u32).saturating_mul(cell_h).max(1);
    let vp_h = if protocol == TerminalImageProtocol::Sixel {
        let safe_h = compute_sixel_safe_pixel_height(view_rows, cell_px.1.max(1));
        if safe_h >= 6 {
            safe_h.min(raw_vp_h)
        } else {
            raw_vp_h
        }
    } else {
        raw_vp_h
    };

    let tall_scroll = is_tall_scrollable(width_px, height_px, vp_w as u64, vp_h as u64, fit.scrollable);
    let base_scale = if tall_scroll {
        (vp_w as f64 / width_px as f64).min(1.0)
    } else {
        (vp_w as f64 / width_px as f64)
            .min(vp_h as f64 / height_px as f64)
            .min(1.0)
    };
    let scale = base_scale * (zoom_pct as f64 / 100.0);

    let raw_w = (width_px as f64 * scale).max(1.0);
    let raw_h = (height_px as f64 * scale).max(1.0);
    let cap_scale = (MAX_RENDER_PX_W as f64 / raw_w)
        .min(MAX_RENDER_PX_H as f64 / raw_h)
        .min(1.0);
    let mut px_w = raw_w * cap_scale;
    let mut px_h = raw_h * cap_scale;

    if protocol == TerminalImageProtocol::Sixel {
        let area = px_w * px_h;
        if area > MAX_SIXEL_PIXELS as f64 {
            let s = (MAX_SIXEL_PIXELS as f64 / area).sqrt();
            px_w *= s;
            px_h *= s;
        }
    }

    let mut target_px_w = (px_w.round() as u32).clamp(1, MAX_RENDER_PX_W);
    let mut target_px_h = (px_h.round() as u32).clamp(1, MAX_RENDER_PX_H);

    if !tall_scroll && zoom_pct <= 100 {
        target_px_w = target_px_w.min(vp_w).max(1);
        target_px_h = target_px_h.min(vp_h).max(1);
        if protocol == TerminalImageProtocol::Sixel && target_px_h.div_ceil(6) * 6 > vp_h && vp_h >= 6 {
            target_px_h = (vp_h - (vp_h % 6)).max(1);
        }
    }

    let occ_cols = target_px_w.div_ceil(cell_w).clamp(1, u16::MAX as u32) as u16;
    let occ_rows = if protocol == TerminalImageProtocol::Sixel && !tall_scroll && zoom_pct <= 100 {
        let band_px_h = target_px_h.div_ceil(6) * 6;
        band_px_h.div_ceil(cell_h).clamp(1, view_rows as u32) as u16
    } else {
        target_px_h.div_ceil(cell_h).clamp(1, u16::MAX as u32) as u16
    };

    (target_px_w, target_px_h, occ_cols, occ_rows)
}

/// Encodes opaque RGBA pixel data (`4 * width * height` bytes) into a standard DEC VT340 / Windows Terminal Sixel stream.
pub fn encode_sixel_rgba(flat_rgba: &[u8], width: u32, height: u32) -> String {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || flat_rgba.len() < w * h * 4 {
        return String::from("\x1bP0;1;q\"1;1;0;0\x1b\\");
    }

    let (palette, indexed) = quantize_rgba_to_256(flat_rgba, w, h);
    encode_sixel_indexed_slice(&palette, &indexed, w, 0, 0, w, h)
}

fn encode_sixel_indexed_slice(
    palette: &[(u8, u8, u8)],
    indexed: &[u8],
    full_w: usize,
    x0: usize,
    y0: usize,
    crop_w: usize,
    crop_h: usize,
) -> String {
    if crop_w == 0 || crop_h == 0 || full_w == 0 || palette.is_empty() {
        return String::from("\x1bP0;1;q\"1;1;0;0\x1b\\");
    }

    let mut out = String::with_capacity(crop_w * crop_h / 2 + palette.len() * 16 + 32);
    let _ = write!(out, "\x1bP0;1;q\"1;1;{crop_w};{crop_h}");

    for (idx, &(r, g, b)) in palette.iter().enumerate() {
        let r_pct = (r as u32 * 100 + 127) / 255;
        let g_pct = (g as u32 * 100 + 127) / 255;
        let b_pct = (b as u32 * 100 + 127) / 255;
        let _ = write!(out, "#{idx};2;{r_pct};{g_pct};{b_pct}");
    }

    let mut color_seen = [false; 256];
    let mut band_colors: Vec<u8> = Vec::with_capacity(palette.len().min(64));
    let mut band_masks = vec![0u8; palette.len().max(1) * crop_w];

    for band_y in (0..crop_h).step_by(6) {
        band_colors.clear();
        let band_h = (crop_h - band_y).min(6);

        for dy in 0..band_h {
            let bit = 1u8 << dy;
            let row_offset = (y0 + band_y + dy) * full_w + x0;
            for x in 0..crop_w {
                let c = indexed[row_offset + x];
                if !color_seen[c as usize] {
                    color_seen[c as usize] = true;
                    band_colors.push(c);
                }
                band_masks[c as usize * crop_w + x] |= bit;
            }
        }

        for (ci, &color_idx) in band_colors.iter().enumerate() {
            if ci > 0 {
                out.push('$');
            }
            let _ = write!(out, "#{color_idx}");

            let mask_start = color_idx as usize * crop_w;
            let row_slice = &band_masks[mask_start..mask_start + crop_w];
            let end = row_slice
                .iter()
                .rposition(|&bits| bits != 0)
                .map_or(0, |idx| idx + 1);

            write_sixel_rle(&row_slice[..end], &mut out);
        }

        for &c in &band_colors {
            color_seen[c as usize] = false;
            let start = c as usize * crop_w;
            band_masks[start..start + crop_w].fill(0);
        }

        if band_y + 6 < crop_h {
            out.push('-');
        }
    }

    out.push_str("\x1b\\");
    out
}

fn write_sixel_rle(masks: &[u8], out: &mut String) {
    let mut i = 0;
    while i < masks.len() {
        let bits = masks[i];
        let mut run_len = 1usize;
        while i + run_len < masks.len() && masks[i + run_len] == bits {
            run_len += 1;
        }
        let ch = (bits + 63) as char;
        if run_len >= 4 {
            let _ = write!(out, "!{run_len}{ch}");
        } else {
            for _ in 0..run_len {
                out.push(ch);
            }
        }
        i += run_len;
    }
}

#[derive(Clone, Copy)]
struct ColorBucket {
    r_avg: u8,
    g_avg: u8,
    b_avg: u8,
    count: u32,
    sum_r: u64,
    sum_g: u64,
    sum_b: u64,
}

/// Perceptual Redmean squared color distance (integer arithmetic, green/luminance weighted).
pub fn redmean_dist_sq(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let r_mean = (r1 as u32 + r2 as u32) / 2;
    let dr = r1.abs_diff(r2) as u32;
    let dg = g1.abs_diff(g2) as u32;
    let db = b1.abs_diff(b2) as u32;
    (((512 + r_mean) * dr * dr) >> 8) + 4 * dg * dg + (((767 - r_mean) * db * db) >> 8)
}

fn quantize_rgba_to_256(
    flat_rgba: &[u8],
    width: usize,
    height: usize,
) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
    let (palette, indexed, _) = quantize_rgba_to_256_with_stats(flat_rgba, width, height);
    (palette, indexed)
}

fn quantize_rgba_to_256_with_stats(
    flat_rgba: &[u8],
    width: usize,
    height: usize,
) -> (Vec<(u8, u8, u8)>, Vec<u8>, usize) {
    let pixel_count = width * height;
    // Fast lossless path when the image uses <= 256 distinct RGB colors.
    let mut exact_map: HashMap<u32, u8> = HashMap::with_capacity(256);
    let mut exact_palette: Vec<(u8, u8, u8)> = Vec::with_capacity(256);
    let mut indexed = Vec::with_capacity(pixel_count);
    let mut overflow = false;

    for px in flat_rgba.as_chunks::<4>().0.iter().take(pixel_count) {
        let key = ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32);
        if let Some(&idx) = exact_map.get(&key) {
            indexed.push(idx);
        } else if exact_palette.len() < 256 {
            let idx = exact_palette.len() as u8;
            exact_map.insert(key, idx);
            exact_palette.push((px[0], px[1], px[2]));
            indexed.push(idx);
        } else {
            overflow = true;
            break;
        }
    }

    if !overflow {
        return (exact_palette, indexed, 0);
    }

    // Perceptual median-cut over 5-bit-per-channel RGB buckets for > 256 distinct colors.
    let mut hist: HashMap<u16, (u32, u64, u64, u64)> = HashMap::new();
    for px in flat_rgba.as_chunks::<4>().0.iter().take(pixel_count) {
        let qkey = (((px[0] as u16) >> 3) << 10)
            | (((px[1] as u16) >> 3) << 5)
            | ((px[2] as u16) >> 3);
        let entry = hist.entry(qkey).or_insert((0, 0, 0, 0));
        entry.0 += 1;
        entry.1 += px[0] as u64;
        entry.2 += px[1] as u64;
        entry.3 += px[2] as u64;
    }

    let mut buckets: Vec<ColorBucket> = hist
        .into_values()
        .map(|(count, sum_r, sum_g, sum_b)| {
            let c = count as u64;
            ColorBucket {
                r_avg: (sum_r / c) as u8,
                g_avg: (sum_g / c) as u8,
                b_avg: (sum_b / c) as u8,
                count,
                sum_r,
                sum_g,
                sum_b,
            }
        })
        .collect();

    let total_buckets = buckets.len();
    let mut boxes: Vec<(usize, usize)> = vec![(0, total_buckets)];

    while boxes.len() < 256 {
        let mut best_box_idx = None;
        let mut best_score = 0u64;
        let mut best_channel = 0u8;

        for (bi, &(start, end)) in boxes.iter().enumerate() {
            if end - start <= 1 {
                continue;
            }
            let (mut min_r, mut max_r) = (u8::MAX, u8::MIN);
            let (mut min_g, mut max_g) = (u8::MAX, u8::MIN);
            let (mut min_b, mut max_b) = (u8::MAX, u8::MIN);
            let mut pop = 0u64;

            for b in &buckets[start..end] {
                min_r = min_r.min(b.r_avg);
                max_r = max_r.max(b.r_avg);
                min_g = min_g.min(b.g_avg);
                max_g = max_g.max(b.g_avg);
                min_b = min_b.min(b.b_avg);
                max_b = max_b.max(b.b_avg);
                pop += b.count as u64;
            }

            let wr = (max_r - min_r) as u64 * 2;
            let wg = (max_g - min_g) as u64 * 4;
            let wb = (max_b - min_b) as u64 * 3;
            let (channel, weighted_span) = if wg >= wr && wg >= wb {
                (1u8, wg)
            } else if wb >= wr {
                (2u8, wb)
            } else {
                (0u8, wr)
            };

            let pop_weight = ((pop as f64).sqrt().ceil() as u64).max(1);
            let score = weighted_span * pop_weight;
            if score > best_score {
                best_score = score;
                best_box_idx = Some(bi);
                best_channel = channel;
            }
        }

        let Some(bi) = best_box_idx else {
            break;
        };
        let (start, end) = boxes[bi];
        let slice = &mut buckets[start..end];
        match best_channel {
            0 => slice.sort_unstable_by_key(|b| b.r_avg),
            1 => slice.sort_unstable_by_key(|b| b.g_avg),
            _ => slice.sort_unstable_by_key(|b| b.b_avg),
        }

        let total_pop: u64 = slice.iter().map(|b| b.count as u64).sum();
        let half_pop = total_pop / 2;
        let mut accum = 0u64;
        let mut split_rel = slice.len() / 2;
        for (idx, b) in slice.iter().enumerate() {
            accum += b.count as u64;
            if accum >= half_pop {
                split_rel = if idx + 1 < slice.len() {
                    idx + 1
                } else {
                    idx.max(1)
                };
                break;
            }
        }
        let mid = start + split_rel.clamp(1, slice.len() - 1);
        boxes[bi] = (start, mid);
        boxes.push((mid, end));
    }

    let raw_palette: Vec<(u8, u8, u8)> = boxes
        .iter()
        .map(|&(start, end)| {
            let mut cnt = 0u64;
            let mut sr = 0u64;
            let mut sg = 0u64;
            let mut sb = 0u64;
            for b in &buckets[start..end] {
                cnt += b.count as u64;
                sr += b.sum_r;
                sg += b.sum_g;
                sb += b.sum_b;
            }
            (
                sr.checked_div(cnt).unwrap_or(0) as u8,
                sg.checked_div(cnt).unwrap_or(0) as u8,
                sb.checked_div(cnt).unwrap_or(0) as u8,
            )
        })
        .collect();

    let mut indexed = vec![0u8; pixel_count];
    let mut lookup_cache: HashMap<u32, u8> = HashMap::with_capacity(4096);
    let mut used = vec![false; raw_palette.len()];
    let mut cur_err = vec![[0.0f32; 3]; width];
    let mut next_err = vec![[0.0f32; 3]; width];

    for y in 0..height {
        let row_base = y * width;
        let ltr = y % 2 == 0;
        for step in 0..width {
            let x = if ltr { step } else { width - 1 - step };
            let px_off = (row_base + x) * 4;
            let orig_r = flat_rgba[px_off];
            let orig_g = flat_rgba[px_off + 1];
            let orig_b = flat_rgba[px_off + 2];

            let r = (orig_r as f32 + cur_err[x][0])
                .round()
                .clamp(0.0, 255.0) as u8;
            let g = (orig_g as f32 + cur_err[x][1])
                .round()
                .clamp(0.0, 255.0) as u8;
            let b = (orig_b as f32 + cur_err[x][2])
                .round()
                .clamp(0.0, 255.0) as u8;

            let key = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let idx = if let Some(&cached) = lookup_cache.get(&key) {
                cached
            } else {
                let mut best_idx = 0u8;
                let mut best_dist = u32::MAX;
                for (pi, &(pr, pg, pb)) in raw_palette.iter().enumerate() {
                    let dist = redmean_dist_sq(r, g, b, pr, pg, pb);
                    if dist < best_dist {
                        best_dist = dist;
                        best_idx = pi as u8;
                        if dist == 0 {
                            break;
                        }
                    }
                }
                if lookup_cache.len() < MAX_QUANT_LOOKUP_CACHE {
                    lookup_cache.insert(key, best_idx);
                }
                best_idx
            };

            let (pr, pg, pb) = raw_palette[idx as usize];
            let raw_er = r as i16 - pr as i16;
            let raw_eg = g as i16 - pg as i16;
            let raw_eb = b as i16 - pb as i16;

            let same_left = x > 0 && {
                let lo = (row_base + x - 1) * 4;
                flat_rgba[lo] == orig_r && flat_rgba[lo + 1] == orig_g && flat_rgba[lo + 2] == orig_b
            };
            let same_up = y > 0 && {
                let uo = ((y - 1) * width + x) * 4;
                flat_rgba[uo] == orig_r && flat_rgba[uo + 1] == orig_g && flat_rgba[uo + 2] == orig_b
            };
            let is_flat = match (x > 0, y > 0) {
                (true, true) => same_left && same_up,
                (true, false) => same_left,
                (false, true) => same_up,
                (false, false) => false,
            };
            let pal_dist = redmean_dist_sq(r, g, b, pr, pg, pb);
            let suppress_dither = (raw_er.abs() <= 3 && raw_eg.abs() <= 3 && raw_eb.abs() <= 3)
                || (is_flat && pal_dist <= 64);

            if !suppress_dither {
                let er = (raw_er as f32).clamp(-48.0, 48.0);
                let eg = (raw_eg as f32).clamp(-48.0, 48.0);
                let eb = (raw_eb as f32).clamp(-48.0, 48.0);

                if ltr {
                    if x + 1 < width {
                        cur_err[x + 1][0] += er * (7.0 / 16.0);
                        cur_err[x + 1][1] += eg * (7.0 / 16.0);
                        cur_err[x + 1][2] += eb * (7.0 / 16.0);
                    }
                    if y + 1 < height {
                        if x > 0 {
                            next_err[x - 1][0] += er * (3.0 / 16.0);
                            next_err[x - 1][1] += eg * (3.0 / 16.0);
                            next_err[x - 1][2] += eb * (3.0 / 16.0);
                        }
                        next_err[x][0] += er * (5.0 / 16.0);
                        next_err[x][1] += eg * (5.0 / 16.0);
                        next_err[x][2] += eb * (5.0 / 16.0);
                        if x + 1 < width {
                            next_err[x + 1][0] += er * (1.0 / 16.0);
                            next_err[x + 1][1] += eg * (1.0 / 16.0);
                            next_err[x + 1][2] += eb * (1.0 / 16.0);
                        }
                    }
                } else {
                    if x > 0 {
                        cur_err[x - 1][0] += er * (7.0 / 16.0);
                        cur_err[x - 1][1] += eg * (7.0 / 16.0);
                        cur_err[x - 1][2] += eb * (7.0 / 16.0);
                    }
                    if y + 1 < height {
                        if x + 1 < width {
                            next_err[x + 1][0] += er * (3.0 / 16.0);
                            next_err[x + 1][1] += eg * (3.0 / 16.0);
                            next_err[x + 1][2] += eb * (3.0 / 16.0);
                        }
                        next_err[x][0] += er * (5.0 / 16.0);
                        next_err[x][1] += eg * (5.0 / 16.0);
                        next_err[x][2] += eb * (5.0 / 16.0);
                        if x > 0 {
                            next_err[x - 1][0] += er * (1.0 / 16.0);
                            next_err[x - 1][1] += eg * (1.0 / 16.0);
                            next_err[x - 1][2] += eb * (1.0 / 16.0);
                        }
                    }
                }
            }

            used[idx as usize] = true;
            indexed[row_base + x] = idx;
        }
        std::mem::swap(&mut cur_err, &mut next_err);
        next_err.fill([0.0; 3]);
    }

    let cache_len = lookup_cache.len();

    // Compact palette so every emitted `#idx;2;...` entry is actually referenced.
    let mut remap = vec![0u8; raw_palette.len()];
    let mut compact_palette = Vec::with_capacity(raw_palette.len());
    for (old_idx, &color) in raw_palette.iter().enumerate() {
        if used[old_idx] {
            remap[old_idx] = compact_palette.len() as u8;
            compact_palette.push(color);
        }
    }
    for idx in &mut indexed {
        *idx = remap[*idx as usize];
    }

    (compact_palette, indexed, cache_len)
}

/// Renders an image for direct CLI terminal stdout (auto-detecting Kitty / iTerm2 / Sixel / 24-bit ANSI Half-Blocks).
pub fn render_ansi_stdout(bytes: &[u8], max_cols: u16, max_rows: u16) -> Result<String> {
    render_ansi_stdout_with_pref(bytes, max_cols, max_rows, ImageProtocolPreference::Auto)
}

/// Renders an image for direct CLI terminal stdout honoring an explicit or auto-detected protocol preference.
pub fn render_ansi_stdout_with_pref(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    pref: ImageProtocolPreference,
) -> Result<String> {
    let caps = detect_graphics_caps(pref);
    render_ansi_stdout_with_caps(bytes, max_cols, max_rows, caps)
}

fn render_ansi_stdout_with_caps(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    caps: TerminalGraphicsCaps,
) -> Result<String> {
    validate_raster_image_bytes(bytes)?;
    let fit = FitBox::fixed(max_cols, max_rows);
    let rasterized_svg;
    let bytes: &[u8] = if looks_like_svg_bytes(bytes) {
        let (max_px_w, max_px_h) = svg_raster_bounds(fit, caps.cell_px);
        rasterized_svg =
            bitefoundry_html::graphics::rasterize_svg_to_png(bytes, max_px_w, max_px_h, None)
                .context("Failed to rasterize SVG")?;
        &rasterized_svg.png_bytes
    } else {
        bytes
    };
    match caps.protocol {
        TerminalImageProtocol::ITerm2 => {
            let guessed = image::guess_format(bytes).ok();
            let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
            let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
            if width_px == 0 || height_px == 0 {
                bail!("Image dimensions are 0x0");
            }
            let (cols, _) = compute_halfblock_dims(width_px, height_px, fit, caps.cell_px);
            let encoded_owned;
            let raw_bytes: &[u8] = if matches!(
                guessed,
                Some(
                    ImageFormat::Png
                        | ImageFormat::Jpeg
                        | ImageFormat::Gif
                        | ImageFormat::WebP
                )
            ) && bytes.len() <= MAX_ITERM2_RAW_BYTES
            {
                bytes
            } else {
                encoded_owned = encode_crisp_png(&dyn_img)?;
                &encoded_owned
            };
            let b64 = B64_STANDARD.encode(raw_bytes);
            Ok(format!(
                "\x1b]1337;File=inline=1;width={};preserveAspectRatio=1:{b64}\x07\n",
                cols
            ))
        }
        TerminalImageProtocol::Kitty => {
            let guessed = image::guess_format(bytes).ok();
            let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
            let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
            if width_px == 0 || height_px == 0 {
                bail!("Image dimensions are 0x0");
            }
            let png_owned;
            let png_bytes: &[u8] = if matches!(guessed, Some(ImageFormat::Png))
                && width_px <= MAX_RENDER_PX_W
                && height_px <= MAX_RENDER_PX_H
            {
                bytes
            } else {
                png_owned = encode_crisp_png(&dyn_img)?;
                &png_owned
            };
            let b64 = B64_STANDARD.encode(png_bytes);
            let chunks: Vec<&str> = b64
                .as_bytes()
                .chunks(4096)
                .map(|c| std::str::from_utf8(c).expect("base64 is valid ASCII"))
                .collect();
            let total = chunks.len();
            let mut out = String::new();
            for (idx, chunk) in chunks.into_iter().enumerate() {
                let more = if idx + 1 < total { 1 } else { 0 };
                if idx == 0 {
                    out.push_str(&format!("\x1b_Gf=100,a=T,m={more};{chunk}\x1b\\"));
                } else {
                    out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
                }
            }
            out.push('\n');
            Ok(out)
        }
        TerminalImageProtocol::Sixel => {
            let rendered = decode_and_render_for_protocol(bytes, fit, (0, 0, 0), caps)?;
            let mut out = rendered.pixel_payload.unwrap_or_default();
            out.push('\n');
            Ok(out)
        }
        TerminalImageProtocol::Halfblock => {
            let rendered =
                decode_and_render_halfblocks_with_cell_px(bytes, fit, (0, 0, 0), caps.cell_px)?;
            let mut out = String::new();
            for line in &rendered.lines {
                for span in &line.spans {
                    let (tr, tg, tb) = match span.style.fg {
                        Some(Color::Rgb(r, g, b)) => (r, g, b),
                        _ => (255, 255, 255),
                    };
                    let (br, bg_c, bb) = match span.style.bg {
                        Some(Color::Rgb(r, g, b)) => (r, g, b),
                        _ => (0, 0, 0),
                    };
                    out.push_str(&format!(
                        "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg_c};{bb}m{}",
                        span.content
                    ));
                }
                out.push_str("\x1b[0m\n");
            }
            Ok(out)
        }
    }
}

fn format_label_for(fmt: Option<ImageFormat>) -> String {
    match fmt {
        Some(ImageFormat::Png) => "PNG".to_string(),
        Some(ImageFormat::Jpeg) => "JPEG".to_string(),
        Some(ImageFormat::Gif) => "GIF".to_string(),
        Some(ImageFormat::WebP) => "WEBP".to_string(),
        Some(other) => format!("{other:?}").to_ascii_uppercase(),
        None => "IMAGE".to_string(),
    }
}

fn blend_over_bg(px: Rgba<u8>, bg: (u8, u8, u8)) -> (u8, u8, u8) {
    let a = px[3] as u16;
    if a == 255 {
        return (px[0], px[1], px[2]);
    }
    if a == 0 {
        return bg;
    }
    let inv = 255 - a;
    let r = ((px[0] as u16 * a + bg.0 as u16 * inv + 127) / 255) as u8;
    let g = ((px[1] as u16 * a + bg.1 as u16 * inv + 127) / 255) as u8;
    let b = ((px[2] as u16 * a + bg.2 as u16 * inv + 127) / 255) as u8;
    (r, g, b)
}

fn encode_crisp_png(dyn_img: &DynamicImage) -> Result<Vec<u8>> {
    let bounded = fit_within_max_bounds(dyn_img, MAX_RENDER_PX_W, MAX_RENDER_PX_H);
    encode_png(&bounded)
}

fn encode_png(img: &DynamicImage) -> Result<Vec<u8>> {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Png)
        .context("Failed to encode PNG")?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, RgbaImage};

    fn make_test_png(w: u32, h: u32) -> Vec<u8> {
        let img: RgbaImage = ImageBuffer::from_fn(w, h, |x, y| {
            if y < h / 2 {
                Rgba([(x * 16) as u8, 120, 40, 255])
            } else {
                Rgba([20, 40, 200, 128])
            }
        });
        let dyn_img = DynamicImage::ImageRgba8(img);
        encode_png(&dyn_img).unwrap()
    }

    #[test]
    fn test_is_image_mime_or_name() {
        assert!(is_image_mime_or_name("image/png", None));
        assert!(is_image_mime_or_name("hostedContent", None));
        assert!(is_image_mime_or_name("application/octet-stream", Some("photo.WEBP")));
        assert!(!is_image_mime_or_name("application/pdf", Some("doc.pdf")));
    }

    #[test]
    fn test_decode_and_render_halfblocks_aspect_coalescing_and_alpha() {
        let png = make_test_png(16, 8);
        let rendered = decode_and_render_halfblocks(&png, 20, 10, (10, 20, 30)).unwrap();
        assert_eq!(rendered.width_px, 16);
        assert_eq!(rendered.height_px, 8);
        assert_eq!(rendered.cols, 16);
        assert_eq!(rendered.rows, 4);
        assert_eq!(rendered.format_label, "PNG");
        assert_eq!(rendered.protocol, TerminalImageProtocol::Halfblock);
        assert!(rendered.pixel_payload.is_none());
        assert_eq!(rendered.lines.len(), 4);

        // Every line must contain half-block characters summing to `cols`
        for line in &rendered.lines {
            let char_count: usize = line
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum();
            assert_eq!(char_count, 16);
            assert!(line.spans.iter().all(|s| s.content.chars().all(|c| c == '▀')));
        }

        // Uniform solid image coalesces every row into a single Span
        let solid: RgbaImage = ImageBuffer::from_pixel(10, 4, Rgba([255, 0, 0, 255]));
        let solid_png = encode_png(&DynamicImage::ImageRgba8(solid)).unwrap();
        let solid_rendered =
            decode_and_render_halfblocks(&solid_png, 20, 10, (0, 0, 0)).unwrap();
        assert_eq!(solid_rendered.lines.len(), 2);
        assert_eq!(solid_rendered.lines[0].spans.len(), 1);
        assert_eq!(solid_rendered.lines[0].spans[0].content, "▀▀▀▀▀▀▀▀▀▀");
    }

    #[test]
    fn test_protocol_auto_detection_matrix() {
        let proto_env_key = ENV_PROTOCOL_KEYS[0];
        // Explicit preference overrides auto-detection when env override is absent or "auto"
        let empty = HashMap::new();
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Kitty, &empty),
            TerminalImageProtocol::Kitty
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Iterm2, &empty),
            TerminalImageProtocol::ITerm2
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Sixel, &empty),
            TerminalImageProtocol::Sixel
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Halfblock, &empty),
            TerminalImageProtocol::Halfblock
        );
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &empty),
            TerminalImageProtocol::Halfblock
        );

        // Explicit env key overrides everything including TMUX and explicit pref
        let mut env_override = HashMap::new();
        env_override.insert("TMUX", "/tmp/tmux-1000/default,1,0");
        env_override.insert(proto_env_key, "sixel");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &env_override),
            TerminalImageProtocol::Sixel
        );
        env_override.insert(proto_env_key, "halfblocks");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Kitty, &env_override),
            TerminalImageProtocol::Halfblock
        );

        // TMUX / STY guard forces Halfblock in Auto mode even when KITTY_WINDOW_ID / WT_SESSION are present
        let mut tmux_env = HashMap::new();
        tmux_env.insert("TMUX", "/tmp/tmux-1000/default,1,0");
        tmux_env.insert("KITTY_WINDOW_ID", "1");
        tmux_env.insert("WT_SESSION", "guid-1");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &tmux_env),
            TerminalImageProtocol::Halfblock
        );

        let mut screen_env = HashMap::new();
        screen_env.insert("STY", "1234.pts-0.host");
        screen_env.insert("TERM", "foot");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &screen_env),
            TerminalImageProtocol::Halfblock
        );

        // Kitty, Ghostty, and WezTerm detection
        let mut kitty_env = HashMap::new();
        kitty_env.insert("KITTY_WINDOW_ID", "42");
        kitty_env.insert("WT_SESSION", "guid-1");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &kitty_env),
            TerminalImageProtocol::Kitty
        );

        let mut ghostty_env = HashMap::new();
        ghostty_env.insert("TERM_PROGRAM", "Ghostty");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &ghostty_env),
            TerminalImageProtocol::Kitty
        );

        // WezTerm + WT_SESSION both set -> Kitty wins
        let mut wez_wt_env = HashMap::new();
        wez_wt_env.insert(
            "WEZTERM_EXECUTABLE",
            "C:\\Program Files\\WezTerm\\wezterm-gui.exe",
        );
        wez_wt_env.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_wt_env),
            TerminalImageProtocol::Kitty
        );

        // iTerm2 terminals keep ITerm2
        let mut iterm_env = HashMap::new();
        iterm_env.insert("TERM_PROGRAM", "iTerm.app");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &iterm_env),
            TerminalImageProtocol::ITerm2
        );

        // WT_SESSION alone stays Halfblock statically so Auto always verifies DA1 ;4
        let mut wt_only = HashMap::new();
        wt_only.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_only),
            TerminalImageProtocol::Halfblock
        );

        // Linux Sixel terminals
        let mut foot_env = HashMap::new();
        foot_env.insert("TERM", "foot");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &foot_env),
            TerminalImageProtocol::Sixel
        );
    }

    #[test]
    fn test_sixel_encoder_solid_and_multiband_and_gradient() {
        // Solid 10x8 red image: single color, RLE `!`, 2 bands (`-` separator), ST terminator
        let solid: RgbaImage = ImageBuffer::from_pixel(10, 8, Rgba([255, 0, 0, 255]));
        let solid_png = encode_png(&DynamicImage::ImageRgba8(solid)).unwrap();
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let rendered = decode_and_render_for_protocol(
            &solid_png,
            FitBox::fixed(40, 20),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        assert_eq!(rendered.protocol, TerminalImageProtocol::Sixel);
        assert!(rendered.lines.is_empty());
        let payload = rendered.pixel_payload.as_deref().unwrap();
        assert!(payload.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(payload.contains("#0;2;100;0;0"));
        assert!(payload.contains('!'));
        assert!(payload.contains('-'));
        assert!(payload.ends_with("\x1b\\"));
        assert!(!payload.ends_with('\n'));
        assert!(rendered.approx_heap_bytes() > std::mem::size_of::<RenderedTerminalImage>());

        // Multi-color (>256 unique RGB colors) gradient tests median-cut quantization path
        let grad: RgbaImage = ImageBuffer::from_fn(32, 16, |x, y| {
            Rgba([
                ((x * 8) % 256) as u8,
                ((y * 16) % 256) as u8,
                (((x + y) * 5) % 256) as u8,
                if y < 8 { 255 } else { 128 },
            ])
        });
        let grad_png = encode_png(&DynamicImage::ImageRgba8(grad)).unwrap();
        let grad_rendered = decode_and_render_for_protocol(
            &grad_png,
            FitBox::fixed(20, 10),
            (12, 24, 36),
            caps,
        )
        .unwrap();
        let grad_payload = grad_rendered.pixel_payload.as_deref().unwrap();
        assert!(grad_payload.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(grad_payload.contains(";2;"));
        assert!(grad_payload.contains('-'));
        assert!(grad_payload.ends_with("\x1b\\"));
    }

    #[test]
    fn test_kitty_and_iterm2_tui_vs_cli_stdout_payloads() {
        let png = make_test_png(16, 8);

        // TUI Kitty payload: fixed image id 4242, q=2, single constraining axis (`c=` only), C=1, no trailing newline
        let kitty_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let kitty_tui = decode_and_render_for_protocol(
            &png,
            FitBox::fixed(20, 10),
            (0, 0, 0),
            kitty_caps,
        )
        .unwrap();
        let kitty_payload = kitty_tui.pixel_payload.as_deref().unwrap();
        assert!(kitty_payload.starts_with(&format!(
            "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={},C=1,m=",
            kitty_tui.cols
        )));
        assert!(!kitty_payload.contains(",r="));
        assert!(kitty_payload.ends_with("\x1b\\"));
        assert!(!kitty_payload.ends_with('\n'));
        assert_eq!(
            kitty_delete_preview_sequence(),
            "\x1b_Ga=d,d=i,i=4242,q=2\x1b\\"
        );

        // TUI iTerm2 payload: doNotMoveCursor=1, no trailing newline
        let iterm_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::ITerm2,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let iterm_tui = decode_and_render_for_protocol(
            &png,
            FitBox::fixed(20, 10),
            (0, 0, 0),
            iterm_caps,
        )
        .unwrap();
        let iterm_payload = iterm_tui.pixel_payload.as_deref().unwrap();
        assert!(iterm_payload.starts_with(&format!(
            "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1;doNotMoveCursor=1:",
            iterm_tui.cols, iterm_tui.rows
        )));
        assert!(iterm_payload.ends_with('\x07'));
        assert!(!iterm_payload.ends_with('\n'));

        // CLI stdout payloads: omit C=1 and doNotMoveCursor=1, end with '\n'
        let kitty_cli = render_ansi_stdout_with_caps(&png, 16, 8, kitty_caps).unwrap();
        assert!(kitty_cli.starts_with("\x1b_Gf=100,a=T,m="));
        assert!(!kitty_cli.contains("C=1"));
        assert!(kitty_cli.ends_with("\x1b\\\n"));

        let iterm_cli = render_ansi_stdout_with_caps(&png, 16, 8, iterm_caps).unwrap();
        assert!(iterm_cli.starts_with("\x1b]1337;File=inline=1;"));
        assert!(!iterm_cli.contains("doNotMoveCursor=1"));
        assert!(iterm_cli.ends_with("\x07\n"));

        let sixel_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let sixel_cli = render_ansi_stdout_with_caps(&png, 16, 8, sixel_caps).unwrap();
        assert!(sixel_cli.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(sixel_cli.ends_with("\x1b\\\n"));
    }

    #[test]
    fn test_render_ansi_stdout_protocols() {
        let png = make_test_png(8, 4);

        let half = render_ansi_stdout_with_caps(
            &png,
            16,
            8,
            TerminalGraphicsCaps {
                protocol: TerminalImageProtocol::Halfblock,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(half.contains("\x1b[38;2;"));
        assert!(half.contains("\x1b[48;2;"));
        assert!(half.contains('▀'));
        assert!(half.ends_with("\x1b[0m\n"));

        let iterm = render_ansi_stdout_with_caps(
            &png,
            16,
            8,
            TerminalGraphicsCaps {
                protocol: TerminalImageProtocol::ITerm2,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(iterm.starts_with("\x1b]1337;File=inline=1;width=8;preserveAspectRatio=1:"));
        assert!(iterm.ends_with("\x07\n"));

        let kitty = render_ansi_stdout_with_caps(
            &png,
            16,
            8,
            TerminalGraphicsCaps {
                protocol: TerminalImageProtocol::Kitty,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(kitty.starts_with("\x1b_Gf=100,a=T,m="));
        assert!(kitty.ends_with("\x1b\\\n"));
    }

    #[test]
    fn test_decode_rejects_invalid_bytes() {
        assert!(decode_and_render_halfblocks(b"not-an-image", 20, 10, (0, 0, 0)).is_err());
        assert!(render_ansi_stdout(b"not-an-image", 20, 10).is_err());
    }

    #[test]
    fn test_approx_heap_bytes_scales_with_dimensions_and_is_nonzero() {
        let empty = RenderedTerminalImage {
            width_px: 0,
            height_px: 0,
            cols: 0,
            rows: 0,
            format_label: String::new(),
            protocol: TerminalImageProtocol::Halfblock,
            lines: Vec::new(),
            pixel_payload: None,
            scroll_backing: None,
        };
        assert_eq!(
            empty.approx_heap_bytes(),
            std::mem::size_of::<RenderedTerminalImage>()
        );
        assert!(empty.approx_heap_bytes() > 0);

        let with_payload = RenderedTerminalImage {
            width_px: 10,
            height_px: 10,
            cols: 2,
            rows: 1,
            format_label: "PNG".to_string(),
            protocol: TerminalImageProtocol::Sixel,
            lines: Vec::new(),
            pixel_payload: Some("x".repeat(512)),
            scroll_backing: None,
        };
        assert!(with_payload.approx_heap_bytes() >= empty.approx_heap_bytes() + 512);

        let png = make_test_png(64, 32);
        let small = decode_and_render_halfblocks(&png, 8, 4, (0, 0, 0)).unwrap();
        let large = decode_and_render_halfblocks(&png, 64, 16, (0, 0, 0)).unwrap();

        let small_cells = (small.cols as usize) * (small.rows as usize);
        let large_cells = (large.cols as usize) * (large.rows as usize);
        assert!(large_cells > small_cells);
        assert!(small.approx_heap_bytes() > empty.approx_heap_bytes());
        assert!(large.approx_heap_bytes() > small.approx_heap_bytes());
    }

    #[test]
    fn test_render_ansi_stdout_rejects_invalid_bytes_all_protocols() {
        for proto in [
            TerminalImageProtocol::Halfblock,
            TerminalImageProtocol::ITerm2,
            TerminalImageProtocol::Kitty,
            TerminalImageProtocol::Sixel,
        ] {
            let caps = TerminalGraphicsCaps {
                protocol: proto,
                ..Default::default()
            };
            assert!(render_ansi_stdout_with_caps(b"bad-bytes", 16, 8, caps).is_err());
        }
    }

    #[test]
    fn test_protocol_helpers_and_cell_size_parser() {
        assert_eq!(TerminalImageProtocol::Kitty.next_manual(), TerminalImageProtocol::ITerm2);
        assert_eq!(TerminalImageProtocol::ITerm2.next_manual(), TerminalImageProtocol::Sixel);
        assert_eq!(TerminalImageProtocol::Sixel.next_manual(), TerminalImageProtocol::Halfblock);
        assert_eq!(TerminalImageProtocol::Halfblock.next_manual(), TerminalImageProtocol::Kitty);

        assert!(TerminalImageProtocol::Kitty.is_pixel());
        assert!(TerminalImageProtocol::ITerm2.is_pixel());
        assert!(TerminalImageProtocol::Sixel.is_pixel());
        assert!(!TerminalImageProtocol::Halfblock.is_pixel());

        assert_eq!(parse_cell_pixel_size("10x20"), Some((10, 20)));
        assert_eq!(parse_cell_pixel_size(" 9X18 "), Some((9, 18)));
        assert_eq!(parse_cell_pixel_size("0x16"), None);
        assert_eq!(parse_cell_pixel_size("bad"), None);
    }

    #[test]
    fn test_terminal_probe_reply_parsers() {
        // CSI 16 t: \x1b[6;<height>;<width>t -> (cell_w, cell_h)
        assert_eq!(parse_csi_cell_size_reply("\x1b[6;20;10t"), Some((10, 20)));
        assert_eq!(
            parse_csi_cell_size_reply("noise\x1b[6;24;12t\x1b[?61;4c"),
            Some((12, 24))
        );
        assert_eq!(parse_csi_cell_size_reply("\x1b[6;0;10t"), None);
        assert_eq!(parse_csi_cell_size_reply("\x1b[4;800;1200t"), None);

        // CSI 14 t: \x1b[4;<height>;<width>t -> (win_w, win_h)
        assert_eq!(
            parse_csi_win_size_reply("\x1b[4;960;1600t"),
            Some((1600, 960))
        );
        assert_eq!(
            parse_csi_win_size_reply("\x1b[6;0;0t\x1b[4;720;1280t\x1b[?62;4c"),
            Some((1280, 720))
        );
        assert_eq!(parse_csi_win_size_reply("\x1b[4;0;1280t"), None);
        assert_eq!(parse_csi_win_size_reply(""), None);

        // Kitty graphics query OK
        assert!(parse_kitty_query_ok("\x1b_Gi=31;OK\x1b\\"));
        assert!(parse_kitty_query_ok("\x1b[6;20;10t\x1b_Gi=31;OK\x1b\\\x1b[?62;4c"));
        assert!(parse_kitty_query_ok("\x1b_Gi=31,r=1;OK\x07"));
        assert!(!parse_kitty_query_ok("\x1b_Gi=31;EINVAL:bad\x1b\\"));
        assert!(!parse_kitty_query_ok("\x1b_Gi=99;OK\x1b\\"));
        assert!(!parse_kitty_query_ok("plain text"));

        // DA1 Sixel attribute (;4)
        assert!(parse_da1_has_sixel("\x1b[?62;1;2;4;6;9;15;22c"));
        assert!(parse_da1_has_sixel("\x1b[6;20;10t\x1b[?61;4c"));
        assert!(!parse_da1_has_sixel("\x1b[?62;1;2;6;9;14;40;42c"));
        assert!(!parse_da1_has_sixel("\x1b[?4c"));
        assert!(!parse_da1_has_sixel(""));
    }

    #[test]
    fn test_kitty_preserves_crisp_resolution_for_jpeg() {
        let img = image::RgbImage::from_fn(400, 240, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let mut jpeg_buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut jpeg_buf, ImageFormat::Jpeg)
            .unwrap();
        let jpeg_bytes = jpeg_buf.into_inner();

        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: false,
        };
        // Even with a small cell viewport (20x10 -> 160x160 px), Kitty re-encodes at crisp 400x240!
        let rendered = decode_and_render_for_protocol(
            &jpeg_bytes,
            FitBox::fixed(20, 10),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        let payload = rendered.pixel_payload.as_deref().unwrap();
        let mut b64 = String::new();
        for chunk in payload.split("\x1b\\") {
            if let Some((_, data)) = chunk.split_once(';') {
                b64.push_str(data);
            }
        }
        let decoded_png = B64_STANDARD.decode(b64).unwrap();
        let decoded_img = image::load_from_memory(&decoded_png).unwrap();
        assert_eq!(decoded_img.width(), 400);
        assert_eq!(decoded_img.height(), 240);
    }

    #[test]
    fn test_floyd_steinberg_sixel_edge_cases_and_gradient() {
        // Edge sizes (1x1, 1x5, 5x1) and >256-color gradient must never panic and emit valid Sixel.
        for (w, h) in [(1u32, 1u32), (1, 5), (5, 1), (40, 24)] {
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for y in 0..h {
                for x in 0..w {
                    rgba.extend_from_slice(&[
                        ((x * 7 + y * 3) % 256) as u8,
                        ((x * 5 + y * 11) % 256) as u8,
                        ((x * 13 + y * 17) % 256) as u8,
                        255,
                    ]);
                }
            }
            let (palette, indexed) = quantize_rgba_to_256(&rgba, w as usize, h as usize);
            assert!(!palette.is_empty() && palette.len() <= 256);
            assert_eq!(indexed.len(), (w * h) as usize);
            assert!(indexed.iter().all(|&i| (i as usize) < palette.len()));

            let sixel = encode_sixel_rgba(&rgba, w, h);
            assert!(sixel.starts_with("\x1bP0;1;q\"1;1;"));
            assert!(sixel.ends_with("\x1b\\"));
        }
    }

    #[test]
    fn test_windows_probe_console_mode_restore_clears_quick_edit_and_sets_extended() {
        // Prev mode with QuickEdit set (0x0040) and ExtendedFlags omitted (0x0000)
        let restored = restore_windows_console_mode_flags(0x0058);
        assert_eq!(restored & WIN_ENABLE_QUICK_EDIT_MODE, 0);
        assert_ne!(restored & WIN_ENABLE_EXTENDED_FLAGS, 0);
        assert_eq!(restored, 0x0098);

        // Prev mode without QuickEdit still includes ENABLE_EXTENDED_FLAGS
        let restored2 = restore_windows_console_mode_flags(0x0018);
        assert_eq!(restored2, 0x0098);
    }

    #[test]
    fn test_html_spoof_rejection_and_native_svg_rendering() {
        let html_doc = b"  \n<!DOCTYPE html><html><body>Sign in</body></html>";
        let html_bom = b"\xEF\xBB\xBF<html><head></head></html>";
        let svg_raw = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"32\" height=\"16\"><rect width=\"32\" height=\"16\" fill=\"#e11d48\"/></svg>";
        let svg_xml = b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<svg viewBox=\"0 0 24 12\"><circle cx=\"12\" cy=\"6\" r=\"5\" fill=\"#2563eb\"/></svg>";
        let png = make_test_png(16, 16);

        assert!(looks_like_html_bytes(html_doc));
        assert!(looks_like_html_bytes(html_bom));
        assert!(!looks_like_html_bytes(svg_raw));
        assert!(!looks_like_html_bytes(&png));

        assert!(looks_like_svg_bytes(svg_raw));
        assert!(looks_like_svg_bytes(svg_xml));
        assert!(!looks_like_svg_bytes(html_doc));
        assert!(!looks_like_svg_bytes(&png));

        // SVG is natively accepted; HTML spoof remains rejected.
        assert!(is_image_mime_or_name("image/svg+xml", Some("icon.svg")));
        assert!(is_image_mime_or_name("image/svg+xml; charset=utf-8", Some("icon.svg")));
        assert!(is_image_mime_or_name("application/octet-stream", Some("vector.svg")));
        assert!(!is_image_mime_or_name("text/html", Some("login.png")));
        assert!(!is_image_mime_or_name("application/xhtml+xml", Some("trick.svg")));
        assert!(is_image_mime_or_name("image/png; charset=binary", None));

        // Standalone SVG renders natively for both Halfblock and pixel protocols with format_label = "SVG".
        let half_svg = decode_and_render_halfblocks(svg_raw, 20, 10, (0, 0, 0)).unwrap();
        assert_eq!(half_svg.format_label, "SVG");
        assert_eq!(half_svg.protocol, TerminalImageProtocol::Halfblock);
        assert!(!half_svg.lines.is_empty());
        assert!(half_svg
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains('▀'))));

        let half_html_err = decode_and_render_halfblocks(html_doc, 20, 10, (0, 0, 0))
            .unwrap_err()
            .to_string();
        assert!(half_html_err.contains("HTML"), "unexpected error: {half_html_err}");

        let sixel_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Sixel,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let sixel_svg = decode_and_render_for_protocol(
            svg_xml,
            FitBox::fixed(20, 10),
            (0, 0, 0),
            sixel_caps,
        )
        .unwrap();
        assert_eq!(sixel_svg.format_label, "SVG");
        assert_eq!(sixel_svg.protocol, TerminalImageProtocol::Sixel);
        assert!(sixel_svg
            .pixel_payload
            .as_deref()
            .is_some_and(|p| p.starts_with("\x1bP")));

        assert!(decode_and_render_for_protocol(
            html_doc,
            FitBox::fixed(20, 10),
            (0, 0, 0),
            sixel_caps
        )
        .unwrap_err()
        .to_string()
        .contains("HTML"));
    }

    #[test]
    fn test_erase_pixel_rect_sequence_and_sixel_safe_height() {
        let seq = erase_pixel_rect_sequence(4, 2, 5, 3, 80, 24);
        assert!(seq.starts_with("\x1b[0m"));
        // Expands by +2 rows (y=2..7 -> 1-based rows 3..=7) and +2 cols (5+2=7 spaces)
        assert!(seq.contains("\x1b[3;5H       "));
        assert!(seq.contains("\x1b[7;5H       "));
        assert!(!seq.contains("\x1b[8;5H"));

        // Clamped to terminal bounds
        let clamped = erase_pixel_rect_sequence(78, 22, 5, 5, 80, 24);
        assert!(clamped.contains("\x1b[23;79H  "));
        assert!(clamped.contains("\x1b[24;79H  "));
        assert!(!clamped.contains("\x1b[25;"));

        assert_eq!(erase_pixel_rect_sequence(0, 0, 0, 5, 80, 24), "");
        assert_eq!(erase_pixel_rect_sequence(0, 0, 5, 0, 80, 24), "");
        assert_eq!(erase_pixel_rect_sequence(80, 5, 4, 4, 80, 24), "");
        assert_eq!(erase_pixel_rect_sequence(5, 24, 4, 4, 80, 24), "");

        assert_eq!(compute_sixel_safe_pixel_height(8, 20), 156); // 160 -> 26 * 6
        assert_eq!(compute_sixel_safe_pixel_height(4, 19), 72); // 76 -> 12 * 6
        assert_eq!(compute_sixel_safe_pixel_height(6, 16), 96); // exact multiple of 6
        assert_eq!(kitty_delete_all_sequence(), "\x1b_Ga=d,d=A,q=2\x1b\\");
    }

    #[test]
    fn test_tall_image_viewport_pixel_payload_for_sixel_kitty_and_iterm2() {
        let grad: RgbaImage = ImageBuffer::from_fn(320, 1600, |x, y| {
            Rgba([
                ((x * 255) / 320) as u8,
                ((y * 255) / 1600) as u8,
                140,
                255,
            ])
        });
        let tall_png = encode_png(&DynamicImage::ImageRgba8(grad)).unwrap();
        for proto in [
            TerminalImageProtocol::Sixel,
            TerminalImageProtocol::Kitty,
            TerminalImageProtocol::ITerm2,
        ] {
            let caps = TerminalGraphicsCaps {
                protocol: proto,
                cell_px: (8, 16),
                cell_px_exact: true,
            };
            let rendered = decode_and_render_for_protocol(
                &tall_png,
                FitBox::new(80, 20, 100, true),
                (20, 20, 20),
                caps,
            )
            .unwrap();
            assert!(
                rendered.rows > 20,
                "expected tall rendered image for {proto:?}, got {} rows",
                rendered.rows
            );
            let (top_payload, top_cols, top_rows) = rendered
                .viewport_pixel_payload(0, 0, 80, 20)
                .expect("top viewport slice");
            let (bot_payload, bot_cols, bot_rows) = rendered
                .viewport_pixel_payload(0, 10, 80, 20)
                .expect("scrolled viewport slice");
            assert!(top_cols <= 80 && top_rows <= 20);
            assert!(bot_cols <= 80 && bot_rows <= 20);
            assert_ne!(top_payload, bot_payload);
            match proto {
                TerminalImageProtocol::Sixel => {
                    assert!(top_payload.starts_with("\x1bP0;1;q\"1;1;"));
                    assert!(bot_payload.ends_with("\x1b\\"));
                }
                TerminalImageProtocol::Kitty => {
                    assert!(top_payload.starts_with("\x1b_Gf=100,a=T,q=2,i=4242,"));
                    assert!(bot_payload.ends_with("\x1b\\"));
                }
                TerminalImageProtocol::ITerm2 => {
                    assert!(top_payload.starts_with("\x1b]1337;File=inline=1;"));
                    assert!(bot_payload.ends_with('\x07'));
                }
                TerminalImageProtocol::Halfblock => unreachable!(),
            }
        }
    }

    // -------------------------------------------------------------------------
    // Track A 1:1 Root-Cause Proof Unit Tests (Plan Section 5, Items 1-7)
    // -------------------------------------------------------------------------

    #[test]
    fn test_probe_vt_mode_enabled_before_query_write_and_restored() {
        use std::cell::RefCell;
        use std::rc::Rc;

        struct RecordingWriter {
            events: Rc<RefCell<Vec<String>>>,
            fail_write: bool,
        }
        impl std::io::Write for RecordingWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.events
                    .borrow_mut()
                    .push(format!("write:{}", String::from_utf8_lossy(buf)));
                if self.fail_write {
                    return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fail"));
                }
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.events.borrow_mut().push("flush".to_string());
                Ok(())
            }
        }

        // Prev mode has QuickEdit (0x0040) set and VTInput (0x0200) cleared.
        let prev_mode = 0x0058u32;
        let events = Rc::new(RefCell::new(Vec::<String>::new()));
        let events_mode = Rc::clone(&events);
        let events_read = Rc::clone(&events);
        let mut writer = RecordingWriter {
            events: Rc::clone(&events),
            fail_write: false,
        };

        let reply = run_probe_with_mode_guard(
            prev_mode,
            move |mode| {
                events_mode.borrow_mut().push(format!("set_mode:0x{mode:04x}"));
            },
            &mut writer,
            "\x1b[16t\x1b[c",
            move || {
                events_read.borrow_mut().push("read_reply".to_string());
                "\x1b[6;20;10t\x1b[?61;4c".to_string()
            },
        );
        assert_eq!(reply, "\x1b[6;20;10t\x1b[?61;4c");

        let log = events.borrow();
        let expected_probe =
            restore_windows_console_mode_flags(prev_mode) | WIN_ENABLE_VIRTUAL_TERMINAL_INPUT;
        let expected_restore = restore_windows_console_mode_flags(prev_mode);
        assert_eq!(expected_probe & WIN_ENABLE_VIRTUAL_TERMINAL_INPUT, WIN_ENABLE_VIRTUAL_TERMINAL_INPUT);
        assert_eq!(expected_restore & WIN_ENABLE_QUICK_EDIT_MODE, 0);
        assert_ne!(expected_restore & WIN_ENABLE_EXTENDED_FLAGS, 0);
        assert_eq!(expected_restore & WIN_ENABLE_VIRTUAL_TERMINAL_INPUT, 0);

        assert_eq!(
            log.as_slice(),
            &[
                format!("set_mode:0x{expected_probe:04x}"),
                "write:\x1b[16t\x1b[c".to_string(),
                "flush".to_string(),
                "read_reply".to_string(),
                format!("set_mode:0x{expected_restore:04x}"),
            ]
        );

        // Even when write fails, RAII guard still restores console mode cleanly.
        let err_events = Rc::new(RefCell::new(Vec::<String>::new()));
        let err_mode = Rc::clone(&err_events);
        let mut fail_writer = RecordingWriter {
            events: Rc::clone(&err_events),
            fail_write: true,
        };
        let empty = run_probe_with_mode_guard(
            prev_mode,
            move |mode| err_mode.borrow_mut().push(format!("set_mode:0x{mode:04x}")),
            &mut fail_writer,
            "\x1b[c",
            || unreachable!("read_reply must not run after write error"),
        );
        assert!(empty.is_empty());
        assert_eq!(
            err_events.borrow().last().map(String::as_str),
            Some(format!("set_mode:0x{expected_restore:04x}").as_str())
        );
    }

    #[test]
    fn test_cell_px_rounded_division_avoids_truncation() {
        // 1590 / 100 = 15.9 -> rounded 16 (not truncated 15); 995 / 50 = 19.9 -> rounded 20
        assert_eq!(compute_rounded_cell_px(1590, 995, 100, 50), Some((16, 20)));
        assert_eq!(compute_rounded_cell_px(1540, 960, 100, 50), Some((15, 19)));
        assert_eq!(compute_rounded_cell_px(0, 995, 100, 50), None);
        assert_eq!(compute_rounded_cell_px(1590, 995, 0, 50), None);

        // Also verified through CSI 14t reply parsing path
        let mut caps = TerminalGraphicsCaps::default();
        let changed = apply_probe_reply(
            &mut caps,
            "\x1b[4;995;1590t\x1b[?61;4c",
            true,
            false,
            Some((100, 50)),
        );
        assert!(changed);
        assert_eq!(caps.cell_px, (16, 20));
        assert!(caps.cell_px_exact);
    }

    #[test]
    fn test_protocol_detection_wt_requires_da1_stackbite_suppressed_wezterm_kitty() {
        // 1. WT_SESSION alone (no DA1 reply) -> Halfblock (never forced Sixel)
        let mut wt_env = HashMap::new();
        wt_env.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        let base_proto = resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_env);
        assert_eq!(base_proto, TerminalImageProtocol::Halfblock);

        // 2. WT_SESSION + DA1 ;4 -> Sixel; WT_SESSION + DA1 without ;4 -> Halfblock
        let mut caps_sixel = TerminalGraphicsCaps {
            protocol: base_proto,
            ..Default::default()
        };
        apply_probe_reply(&mut caps_sixel, "\x1b[?61;4c", false, true, None);
        assert_eq!(caps_sixel.protocol, TerminalImageProtocol::Sixel);

        let mut caps_no_sixel = TerminalGraphicsCaps {
            protocol: base_proto,
            ..Default::default()
        };
        apply_probe_reply(&mut caps_no_sixel, "\x1b[?61;1c", false, true, None);
        assert_eq!(caps_no_sixel.protocol, TerminalImageProtocol::Halfblock);

        // 3. WT_SESSION + STACKBITE_ROOM_ID -> static path stays Halfblock; probe reply wins
        let mut sb_env = HashMap::new();
        sb_env.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        sb_env.insert("STACKBITE_ROOM_ID", "room-123");
        sb_env.insert("WEZTERM_EXECUTABLE", "wezterm-gui.exe");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &sb_env),
            TerminalImageProtocol::Halfblock
        );
        let mut sb_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Halfblock,
            ..Default::default()
        };
        apply_probe_reply(&mut sb_caps, "\x1b_Gi=31;OK\x1b\\\x1b[?61;1c", false, true, None);
        assert_eq!(sb_caps.protocol, TerminalImageProtocol::Kitty);

        // 4. WezTerm (WEZTERM_EXECUTABLE / TERM_PROGRAM=wezterm) -> Kitty (beats ITerm2)
        let mut wez_exec = HashMap::new();
        wez_exec.insert("WEZTERM_EXECUTABLE", "C:\\Program Files\\WezTerm\\wezterm-gui.exe");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_exec),
            TerminalImageProtocol::Kitty
        );
        let mut wez_prog = HashMap::new();
        wez_prog.insert("TERM_PROGRAM", "wezterm");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_prog),
            TerminalImageProtocol::Kitty
        );
    }

    #[test]
    fn test_kitty_single_axis_emits_only_constraining_dimension() {
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (10, 20),
            cell_px_exact: true,
        };

        // Width-constrained landscape (1600x900 in 80x40 cells): emits `c=` only, NO `,r=`
        let wide_png = encode_png(&DynamicImage::ImageRgba8(ImageBuffer::from_pixel(
            1600,
            900,
            Rgba([40, 120, 200, 255]),
        )))
        .unwrap();
        let wide = decode_and_render_for_protocol(
            &wide_png,
            FitBox::fixed(80, 40),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        let wide_payload = wide.pixel_payload.as_deref().unwrap();
        let wide_hdr = wide_payload.split(';').next().unwrap();
        assert!(wide_hdr.contains(",c="), "expected c= in {wide_hdr}");
        assert!(!wide_hdr.contains(",r="), "unexpected r= in {wide_hdr}");

        // Height-constrained portrait (400x1200 in 80x20 cells): emits `r=` only, NO `,c=`
        let tall_png = encode_png(&DynamicImage::ImageRgba8(ImageBuffer::from_pixel(
            400,
            1200,
            Rgba([200, 80, 40, 255]),
        )))
        .unwrap();
        let tall = decode_and_render_for_protocol(
            &tall_png,
            FitBox::fixed(80, 20),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        let tall_payload = tall.pixel_payload.as_deref().unwrap();
        let tall_hdr = tall_payload.split(';').next().unwrap();
        assert!(tall_hdr.contains(",r="), "expected r= in {tall_hdr}");
        assert!(!tall_hdr.contains(",c="), "unexpected c= in {tall_hdr}");
    }

    #[test]
    fn test_pixel_scroll_backing_crops_native_source_without_double_scaling() {
        let img: RgbaImage = ImageBuffer::from_fn(800, 2400, |x, y| {
            Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
        });
        let png = encode_png(&DynamicImage::ImageRgba8(img)).unwrap();
        let caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (10, 20),
            cell_px_exact: true,
        };
        let rendered = decode_and_render_for_protocol(
            &png,
            FitBox::new(40, 20, 100, true),
            (0, 0, 0),
            caps,
        )
        .unwrap();
        assert_eq!(rendered.cols, 40);
        assert!(rendered.rows > 20);

        let (slice_payload, draw_cols, draw_rows) = rendered
            .viewport_pixel_payload(0, 10, 40, 20)
            .expect("viewport slice");
        assert_eq!(draw_cols, 40);
        assert_eq!(draw_rows, 20);

        let mut b64 = String::new();
        for chunk in slice_payload.split("\x1b\\") {
            if let Some((_, data)) = chunk.split_once(';') {
                b64.push_str(data);
            }
        }
        let decoded_png = B64_STANDARD.decode(b64).unwrap();
        let decoded_img = image::load_from_memory(&decoded_png).unwrap();
        // Width stays at native source 800px, NOT pre-downscaled 400px!
        assert_eq!(decoded_img.width(), 800);
        assert_eq!(decoded_img.height(), 800);
    }

    #[test]
    fn test_fit_box_no_forced_crop_on_wide_low_terminal_at_100pct() {
        let fit = FitBox::new(176, 25, 100, true);
        for proto in [
            TerminalImageProtocol::Kitty,
            TerminalImageProtocol::ITerm2,
            TerminalImageProtocol::Sixel,
        ] {
            for cell_px in [(8, 16), (10, 20)] {
                let (_, _, occ_cols, occ_rows) =
                    compute_pixel_fit_for_protocol(1920, 1080, fit, cell_px, proto);
                assert!(
                    occ_rows <= 25 && occ_cols <= 176,
                    "forced crop at 100% zoom for {proto:?} cell_px={cell_px:?}: got {occ_cols}x{occ_rows}"
                );
            }
        }
        let (hb_cols, hb_subpx_h) = compute_halfblock_dims(1920, 1080, fit, (10, 20));
        assert!(hb_cols <= 176 && hb_subpx_h / 2 <= 25);
    }

    #[test]
    fn test_tall_screenshot_uses_full_width_past_92_cols_and_zoom_300pct_scales() {
        let cell_px = (10, 20);
        let (w100, h100, c100, r100) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::new(140, 30, 100, true),
            cell_px,
            TerminalImageProtocol::Kitty,
        );
        assert!(c100 > 92, "expected full width past 92 cols, got {c100}");
        assert!(r100 > 30);

        let (w200, h200, c200, r200) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::new(140, 30, 200, true),
            cell_px,
            TerminalImageProtocol::Kitty,
        );
        let (w300, h300, c300, r300) = compute_pixel_fit_for_protocol(
            1200,
            4000,
            FitBox::new(140, 30, 300, true),
            cell_px,
            TerminalImageProtocol::Kitty,
        );

        assert!(w200 > w100 && h200 > h100);
        assert!(w300 > w200 && h300 > h200);
        assert!(c200 > c100 && r200 > r100);
        assert!(c300 > c200 && r300 > r200);
        // Must not be capped at the old 1920px width or 3600px height limits
        assert!(w200 > 1920 && h200 > 3600);
        assert!(h300 <= MAX_RENDER_PX_H && w300 <= MAX_RENDER_PX_W);
    }

    #[test]
    fn test_sixel_preserves_ui_accent_suppresses_flat_dither_and_bounds_cache() {
        // Synthetic UI screenshot: 98% flat gray (240, 240, 240) + ~2% vivid blue/red text
        // plus a small >256-color avatar gradient patch so median-cut quantization executes.
        let w = 200usize;
        let h = 100usize;
        let mut rgba = vec![0u8; w * h * 4];
        let bg = (240u8, 240u8, 240u8);
        let blue = (0u8, 90u8, 255u8);
        let red = (230u8, 20u8, 30u8);

        for y in 0..h {
            for x in 0..w {
                let off = (y * w + x) * 4;
                let (r, g, b) = if (10..14).contains(&y) && (10..60).contains(&x) {
                    blue
                } else if (20..24).contains(&y) && (10..60).contains(&x) {
                    red
                } else if (30..46).contains(&y) && (10..28).contains(&x) {
                    // 16x18 = 288 distinct RGB colors in a small avatar patch
                    (((x - 10) * 14) as u8, ((y - 30) * 15) as u8, (((x + y) * 9) % 256) as u8)
                } else {
                    bg
                };
                rgba[off] = r;
                rgba[off + 1] = g;
                rgba[off + 2] = b;
                rgba[off + 3] = 255;
            }
        }

        let (palette, indexed, _) = quantize_rgba_to_256_with_stats(&rgba, w, h);
        // (a) Both vivid UI accent colors are preserved closely in the palette
        let min_blue_dist = palette
            .iter()
            .map(|&(r, g, b)| redmean_dist_sq(r, g, b, blue.0, blue.1, blue.2))
            .min()
            .unwrap();
        let min_red_dist = palette
            .iter()
            .map(|&(r, g, b)| redmean_dist_sq(r, g, b, red.0, red.1, red.2))
            .min()
            .unwrap();
        assert!(
            min_blue_dist <= 200,
            "blue accent lost from palette (min dist {min_blue_dist})"
        );
        assert!(
            min_red_dist <= 200,
            "red accent lost from palette (min dist {min_red_dist})"
        );

        // (b) Interior flat background region maps to a single uniform palette index (0 dither noise)
        let ref_bg_idx = indexed[60 * w + 100];
        for y in 60..95 {
            for x in 100..195 {
                assert_eq!(
                    indexed[y * w + x],
                    ref_bg_idx,
                    "dither noise on flat background at ({x},{y})"
                );
            }
        }

        // (c) Large >20,000-unique-color gradient keeps lookup_cache bounded <= MAX_QUANT_LOOKUP_CACHE
        let gw = 200usize;
        let gh = 150usize;
        let mut grad = Vec::with_capacity(gw * gh * 4);
        for y in 0..gh {
            for x in 0..gw {
                grad.extend_from_slice(&[
                    ((x * 255) / gw) as u8,
                    ((y * 255) / gh) as u8,
                    (((x * 3 + y * 5) * 255) / (gw * 3 + gh * 5)) as u8,
                    255,
                ]);
            }
        }
        let (_, _, cache_len) = quantize_rgba_to_256_with_stats(&grad, gw, gh);
        assert!(
            cache_len <= MAX_QUANT_LOOKUP_CACHE,
            "lookup_cache exceeded cap: {cache_len} > {MAX_QUANT_LOOKUP_CACHE}"
        );
    }

    #[test]
    fn test_halfblock_aspect_ratio_matches_actual_cell_px() {
        let sq: RgbaImage = ImageBuffer::from_pixel(200, 200, Rgba([60, 140, 220, 255]));
        let png = encode_png(&DynamicImage::ImageRgba8(sq)).unwrap();
        let rendered = decode_and_render_halfblocks_with_cell_px(
            &png,
            FitBox::fixed(60, 30),
            (20, 22, 26),
            (9, 22),
        )
        .unwrap();
        let phys_ratio = (rendered.cols as f64 * 9.0) / (rendered.rows as f64 * 22.0);
        assert!(
            (0.90..=1.10).contains(&phys_ratio),
            "expected physical ratio in 0.90..=1.10, got {phys_ratio:.3} (cols={}, rows={})",
            rendered.cols,
            rendered.rows
        );
    }

    #[test]
    fn test_inline_halfblock_uses_light_theme_bg_for_transparent_png() {
        let transparent: RgbaImage = ImageBuffer::from_pixel(16, 8, Rgba([0, 0, 0, 0]));
        let png = encode_png(&DynamicImage::ImageRgba8(transparent)).unwrap();
        let light_bg = color_to_rgb(Color::Rgb(245, 247, 250), (20, 22, 26));
        assert_eq!(light_bg, (245, 247, 250));
        assert_eq!(color_to_rgb(Color::Reset, (245, 247, 250)), (245, 247, 250));

        let rendered = decode_and_render_halfblocks_with_cell_px(
            &png,
            FitBox::fixed(24, 16),
            light_bg,
            (10, 20),
        )
        .unwrap();
        assert!(!rendered.lines.is_empty());
        for line in &rendered.lines {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(Color::Rgb(245, 247, 250)));
                assert_eq!(span.style.bg, Some(Color::Rgb(245, 247, 250)));
            }
        }
    }
}
