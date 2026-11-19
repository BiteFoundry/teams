//! Inline terminal image preview renderer using `image` (PNG/JPEG/GIF/WebP) ->
//! Truecolor Unicode Half-Blocks (`▀` U+2580) for `ratatui` + Kitty / iTerm2 / Sixel / ANSI Half-Block rendering for TUI & CLI.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, IsTerminal, Write as _};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64_STANDARD, Engine as _};
use image::{imageops::FilterType, DynamicImage, ImageFormat, Rgba};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::config::ImageProtocolPreference;

/// Upper memory budget (8 MiB) for decoded terminal inline image caches.
pub const MAX_IMAGE_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Fixed Kitty graphics image ID used by the TUI preview overlay so prior placements can be cleanly deleted.
pub const KITTY_PREVIEW_IMAGE_ID: u32 = 4242;

const MAX_PIXEL_WIDTH: u32 = 1920;
const MAX_PIXEL_HEIGHT: u32 = 1200;
const MAX_ITERM2_RAW_BYTES: usize = 2 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_millis(80);

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
}

impl RenderedTerminalImage {
    /// Approximate heap footprint of this rendered terminal image in bytes.
    pub fn approx_heap_bytes(&self) -> usize {
        let mut total = std::mem::size_of::<Self>()
            + self.format_label.capacity()
            + self.lines.capacity() * std::mem::size_of::<Line<'static>>()
            + self.pixel_payload.as_ref().map_or(0, |s| s.capacity());
        for line in &self.lines {
            total += line.spans.capacity() * std::mem::size_of::<Span<'static>>();
            for span in &line.spans {
                total += span.content.len();
            }
        }
        total
    }
}

/// Returns the Kitty graphics delete sequence for the fixed TUI preview placement (`q=2` suppresses terminal responses).
pub fn kitty_delete_preview_sequence() -> &'static str {
    "\x1b_Ga=d,d=i,i=4242,q=2\x1b\\"
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

/// Actively probes terminal graphics capabilities and exact cell pixel size once on TUI startup
/// while the terminal is in raw mode (non-blocking, bounded to ~80ms).
pub fn probe_terminal_graphics_caps(pref: ImageProtocolPreference) -> TerminalGraphicsCaps {
    let mut caps = detect_graphics_caps(pref);

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return caps;
    }

    let get_env = |k: &str| std::env::var(k).ok();
    let has_explicit_env_protocol = non_empty_env(&get_env, "TEAMS_IMAGE_PROTOCOL")
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

    {
        let mut stdout = std::io::stdout();
        if stdout.write_all(query.as_bytes()).is_err() || stdout.flush().is_err() {
            return caps;
        }
    }

    let reply = read_terminal_probe_reply(PROBE_TIMEOUT);
    if reply.is_empty() {
        return caps;
    }

    if need_cell_px {
        if let Some(cell_px) = parse_csi_cell_size_reply(&reply) {
            caps.cell_px = cell_px;
            caps.cell_px_exact = true;
        } else if let Some((win_w, win_h)) = parse_csi_win_size_reply(&reply) {
            if let Ok((cols, rows)) = crossterm::terminal::size() {
                if cols > 0 && rows > 0 && win_w >= cols && win_h >= rows {
                    let cw = (win_w / cols).clamp(4, 64);
                    let ch = (win_h / rows).clamp(8, 128);
                    caps.cell_px = (cw, ch);
                    caps.cell_px_exact = true;
                }
            }
        }
    }

    if need_proto {
        if parse_kitty_query_ok(&reply) {
            caps.protocol = TerminalImageProtocol::Kitty;
        } else if parse_da1_has_sixel(&reply) {
            caps.protocol = TerminalImageProtocol::Sixel;
        }
    }

    caps
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
            if !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit() || b == b';') {
                if body.split(';').skip(1).any(|attr| attr == "4") {
                    return true;
                }
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

#[cfg(windows)]
fn read_terminal_probe_reply(timeout: Duration) -> String {
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

    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> isize;
        fn WaitForSingleObject(h_handle: isize, dw_milliseconds: u32) -> u32;
        fn GetNumberOfConsoleInputEvents(h_console_input: isize, lpc_number_of_events: *mut u32)
            -> i32;
        fn ReadConsoleInputW(
            h_console_input: isize,
            lp_buffer: *mut InputRecord,
            n_length: u32,
            lp_number_of_events_read: *mut u32,
        ) -> i32;
    }

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle == 0 || handle == -1 {
        return String::new();
    }

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
        if unsafe { ReadConsoleInputW(handle, records.as_mut_ptr(), to_read, &mut read_count) } == 0
            || read_count == 0
        {
            break;
        }
        for rec in &records[..read_count as usize] {
            if rec.event_type == KEY_EVENT {
                let key = unsafe { rec.event.key };
                if key.b_key_down != 0 && key.unicode_char > 0 && key.unicode_char <= 0x7f {
                    let ch = key.unicode_char as u8 as char;
                    let reps = key.w_repeat_count.max(1).min(16);
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

    reply
}

#[cfg(unix)]
fn read_terminal_probe_reply(timeout: Duration) -> String {
    use std::io::Read as _;
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
fn read_terminal_probe_reply(_timeout: Duration) -> String {
    String::new()
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
    if let Some(raw) = non_empty_env(&get_env, "TEAMS_IMAGE_PROTOCOL") {
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

    let term_program = lower_env(&get_env, "TERM_PROGRAM");
    let term = lower_env(&get_env, "TERM");

    // Kitty Graphics Protocol (checked first so Kitty/Ghostty launched from another shell always wins).
    if non_empty_env(&get_env, "KITTY_WINDOW_ID").is_some()
        || non_empty_env(&get_env, "GHOSTTY_RESOURCES_DIR").is_some()
        || matches!(term_program.as_str(), "kitty" | "ghostty")
        || term.contains("xterm-kitty")
        || term.contains("ghostty")
    {
        return TerminalImageProtocol::Kitty;
    }

    // iTerm2 Inline Images (`OSC 1337`), checked before `WT_SESSION` so WezTerm/Rio/Mintty win under Windows Terminal.
    if non_empty_env(&get_env, "WEZTERM_EXECUTABLE").is_some()
        || non_empty_env(&get_env, "WEZTERM_PANE").is_some()
        || matches!(
            term_program.as_str(),
            "wezterm" | "iterm.app" | "rio" | "mintty" | "hyper"
        )
    {
        return TerminalImageProtocol::ITerm2;
    }

    // Sixel (`DCS q`)
    if matches!(
        term.as_str(),
        "foot" | "foot-extra" | "mlterm" | "yaft-256color"
    ) || matches!(term_program.as_str(), "foot" | "contour")
    {
        return TerminalImageProtocol::Sixel;
    }

    if non_empty_env(&get_env, "WT_SESSION").is_some()
        && !matches!(term_program.as_str(), "vscode" | "cursor" | "windsurf")
        && non_empty_env(&get_env, "VSCODE_PID").is_none()
        && non_empty_env(&get_env, "VSCODE_INJECTION").is_none()
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
    if let Ok(raw) = std::env::var("TEAMS_CELL_PIXEL_SIZE") {
        if let Some(parsed) = parse_cell_pixel_size(&raw) {
            return (parsed, true);
        }
    }
    if let Ok(ws) = crossterm::terminal::window_size() {
        if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 {
            let cw = (ws.width / ws.columns).clamp(4, 64);
            let ch = (ws.height / ws.rows).clamp(8, 128);
            return ((cw, ch), true);
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

/// Returns true if a MIME type or filename looks like a supported raster image (`png`, `jpeg`, `jpg`, `gif`, `webp`).
pub fn is_image_mime_or_name(content_type: &str, file_name: Option<&str>) -> bool {
    let ct = content_type.trim().to_ascii_lowercase();
    if matches!(
        ct.as_str(),
        "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp" | "hostedcontent"
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
        {
            return true;
        }
    }
    false
}

/// Decodes an image buffer and renders aspect-ratio-preserving Truecolor Unicode Half-Blocks (`▀`) for `ratatui`.
pub fn decode_and_render_halfblocks(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    bg_rgb: (u8, u8, u8),
) -> Result<RenderedTerminalImage> {
    let guessed = image::guess_format(bytes).ok();
    let format_label = format_label_for(guessed);
    let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
    let rgba = dyn_img.to_rgba8();
    let (width_px, height_px) = rgba.dimensions();
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_w, target_h) = compute_target_dims(width_px, height_px, max_cols, max_rows);

    let resized = image::imageops::resize(&rgba, target_w, target_h, FilterType::Lanczos3);
    let mut lines = Vec::with_capacity((target_h / 2) as usize);

    for y in (0..target_h).step_by(2) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut current_pair: Option<((u8, u8, u8), (u8, u8, u8))> = None;
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
    })
}

/// Decodes and renders an image using the requested terminal graphics protocol (`Kitty`, `ITerm2`, `Sixel`, or `Halfblock`).
pub fn decode_and_render_for_protocol(
    bytes: &[u8],
    max_cols: u16,
    max_rows: u16,
    bg_rgb: (u8, u8, u8),
    caps: TerminalGraphicsCaps,
) -> Result<RenderedTerminalImage> {
    if caps.protocol == TerminalImageProtocol::Halfblock {
        return decode_and_render_halfblocks(bytes, max_cols, max_rows, bg_rgb);
    }

    let guessed = image::guess_format(bytes).ok();
    let format_label = format_label_for(guessed);
    let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
    let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
    if width_px == 0 || height_px == 0 {
        bail!("Image dimensions are 0x0");
    }

    let (target_px_w, target_px_h, occ_cols, occ_rows) =
        compute_pixel_fit(width_px, height_px, max_cols, max_rows, caps.cell_px);

    let payload = match caps.protocol {
        TerminalImageProtocol::Kitty => {
            let png_owned;
            let png_bytes: &[u8] = if matches!(guessed, Some(ImageFormat::Png))
                && width_px <= MAX_PIXEL_WIDTH
                && height_px <= MAX_PIXEL_HEIGHT
            {
                bytes
            } else {
                png_owned = encode_crisp_png(&dyn_img)?;
                &png_owned
            };
            encode_kitty_tui_payload(png_bytes, occ_cols, occ_rows)
        }
        TerminalImageProtocol::ITerm2 => {
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
            format!(
                "\x1b]1337;File=inline=1;width={occ_cols};height={occ_rows};preserveAspectRatio=1;doNotMoveCursor=1:{b64}\x07"
            )
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
            encode_sixel_rgba(&flat_rgba, target_px_w, target_px_h)
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
    })
}

fn encode_kitty_tui_payload(png_bytes: &[u8], occ_cols: u16, occ_rows: u16) -> String {
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
            let _ = write!(
                out,
                "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={occ_cols},r={occ_rows},C=1,m={more};{chunk}\x1b\\"
            );
        } else {
            let _ = write!(out, "\x1b_Gq=2,m={more};{chunk}\x1b\\");
        }
    }
    out
}

fn compute_target_dims(width_px: u32, height_px: u32, max_cols: u16, max_rows: u16) -> (u32, u32) {
    let clamped_cols = max_cols.max(4);
    let clamped_rows = max_rows.max(2);
    let max_px_w = clamped_cols as u32;
    let max_px_h = (clamped_rows as u32) * 2;

    let scale = (max_px_w as f64 / width_px as f64)
        .min(max_px_h as f64 / height_px as f64)
        .min(1.0);
    let target_w = ((width_px as f64 * scale).round() as u32).clamp(1, max_px_w);
    let mut target_h = ((height_px as f64 * scale).round() as u32).clamp(2, max_px_h);
    if target_h % 2 != 0 {
        if target_h + 1 <= max_px_h {
            target_h += 1;
        } else {
            target_h = target_h.saturating_sub(1).max(2);
        }
    }
    (target_w, target_h)
}

fn compute_pixel_fit(
    width_px: u32,
    height_px: u32,
    max_cols: u16,
    max_rows: u16,
    cell_px: (u16, u16),
) -> (u32, u32, u16, u16) {
    let cell_w = cell_px.0.max(1);
    let cell_h = cell_px.1.max(1);
    let clamped_cols = max_cols.max(4);
    let clamped_rows = max_rows.max(2);

    let box_px_w = clamped_cols as f64 * cell_w as f64;
    let box_px_h = clamped_rows as f64 * cell_h as f64;
    let scale = (box_px_w / width_px as f64)
        .min(box_px_h / height_px as f64)
        .min(2.0);

    let max_target_w = (clamped_cols as u32 * cell_w as u32)
        .min(MAX_PIXEL_WIDTH)
        .max(1);
    let max_target_h = (clamped_rows as u32 * cell_h as u32)
        .min(MAX_PIXEL_HEIGHT)
        .max(1);

    let target_px_w = ((width_px as f64 * scale).round() as u32).clamp(1, max_target_w);
    let target_px_h = ((height_px as f64 * scale).round() as u32).clamp(1, max_target_h);

    let occ_cols = (target_px_w.div_ceil(cell_w as u32)).clamp(1, max_cols.max(1) as u32) as u16;
    let occ_rows = (target_px_h.div_ceil(cell_h as u32)).clamp(1, max_rows.max(1) as u32) as u16;

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
    let mut out = String::with_capacity(w * h / 2 + palette.len() * 16 + 32);
    let _ = write!(out, "\x1bP0;1;q\"1;1;{width};{height}");

    for (idx, &(r, g, b)) in palette.iter().enumerate() {
        let r_pct = (r as u32 * 100 + 127) / 255;
        let g_pct = (g as u32 * 100 + 127) / 255;
        let b_pct = (b as u32 * 100 + 127) / 255;
        let _ = write!(out, "#{idx};2;{r_pct};{g_pct};{b_pct}");
    }

    let mut color_seen = [false; 256];
    let mut band_colors: Vec<u8> = Vec::with_capacity(palette.len().min(64));
    let mut band_masks = vec![0u8; palette.len().max(1) * w];

    for band_y in (0..h).step_by(6) {
        band_colors.clear();
        let band_h = (h - band_y).min(6);

        for dy in 0..band_h {
            let bit = 1u8 << dy;
            let row_offset = (band_y + dy) * w;
            for x in 0..w {
                let c = indexed[row_offset + x];
                if !color_seen[c as usize] {
                    color_seen[c as usize] = true;
                    band_colors.push(c);
                }
                band_masks[c as usize * w + x] |= bit;
            }
        }

        for (ci, &color_idx) in band_colors.iter().enumerate() {
            if ci > 0 {
                out.push('$');
            }
            let _ = write!(out, "#{color_idx}");

            let mask_start = color_idx as usize * w;
            let row_slice = &band_masks[mask_start..mask_start + w];
            let end = row_slice
                .iter()
                .rposition(|&bits| bits != 0)
                .map_or(0, |idx| idx + 1);

            write_sixel_rle(&row_slice[..end], &mut out);
        }

        for &c in &band_colors {
            color_seen[c as usize] = false;
            let start = c as usize * w;
            band_masks[start..start + w].fill(0);
        }

        if band_y + 6 < h {
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

fn quantize_rgba_to_256(
    flat_rgba: &[u8],
    width: usize,
    height: usize,
) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
    let pixel_count = width * height;
    // Fast lossless path when the image uses <= 256 distinct RGB colors.
    let mut exact_map: HashMap<u32, u8> = HashMap::with_capacity(256);
    let mut exact_palette: Vec<(u8, u8, u8)> = Vec::with_capacity(256);
    let mut indexed = Vec::with_capacity(pixel_count);
    let mut overflow = false;

    for px in flat_rgba.chunks_exact(4).take(pixel_count) {
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
        return (exact_palette, indexed);
    }

    // Median-cut over 5-bit-per-channel RGB buckets for > 256 distinct colors.
    let mut hist: HashMap<u16, (u32, u64, u64, u64)> = HashMap::new();
    for px in flat_rgba.chunks_exact(4).take(pixel_count) {
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

            let span_r = max_r - min_r;
            let span_g = max_g - min_g;
            let span_b = max_b - min_b;
            let (channel, max_span) = if span_r >= span_g && span_r >= span_b {
                (0u8, span_r)
            } else if span_g >= span_b {
                (1u8, span_g)
            } else {
                (2u8, span_b)
            };

            let score = (max_span as u64) * pop;
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
        let mut split_rel = 1usize;
        for (idx, b) in slice.iter().enumerate() {
            accum += b.count as u64;
            if accum >= half_pop && idx + 1 < slice.len() {
                split_rel = (idx + 1).max(1);
                break;
            }
        }
        let mid = start + split_rel;
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
            if cnt == 0 {
                (0, 0, 0)
            } else {
                ((sr / cnt) as u8, (sg / cnt) as u8, (sb / cnt) as u8)
            }
        })
        .collect();

    indexed.clear();
    let mut lookup_cache: HashMap<u32, u8> = HashMap::with_capacity(4096);
    let mut used = vec![false; raw_palette.len()];
    let mut cur_err = vec![[0.0f32; 3]; width];
    let mut next_err = vec![[0.0f32; 3]; width];

    for y in 0..height {
        let row_base = y * width;
        for x in 0..width {
            let px_off = (row_base + x) * 4;
            let r = (flat_rgba[px_off] as f32 + cur_err[x][0])
                .round()
                .clamp(0.0, 255.0) as u8;
            let g = (flat_rgba[px_off + 1] as f32 + cur_err[x][1])
                .round()
                .clamp(0.0, 255.0) as u8;
            let b = (flat_rgba[px_off + 2] as f32 + cur_err[x][2])
                .round()
                .clamp(0.0, 255.0) as u8;

            let key = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
            let idx = if let Some(&cached) = lookup_cache.get(&key) {
                cached
            } else {
                let mut best_idx = 0u8;
                let mut best_dist = i32::MAX;
                for (pi, &(pr, pg, pb)) in raw_palette.iter().enumerate() {
                    let dr = r as i32 - pr as i32;
                    let dg = g as i32 - pg as i32;
                    let db = b as i32 - pb as i32;
                    let dist = dr * dr + dg * dg + db * db;
                    if dist < best_dist {
                        best_dist = dist;
                        best_idx = pi as u8;
                        if dist == 0 {
                            break;
                        }
                    }
                }
                lookup_cache.insert(key, best_idx);
                best_idx
            };

            let (pr, pg, pb) = raw_palette[idx as usize];
            let er = (r as f32 - pr as f32).clamp(-64.0, 64.0);
            let eg = (g as f32 - pg as f32).clamp(-64.0, 64.0);
            let eb = (b as f32 - pb as f32).clamp(-64.0, 64.0);

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

            used[idx as usize] = true;
            indexed.push(idx);
        }
        std::mem::swap(&mut cur_err, &mut next_err);
        next_err.fill([0.0; 3]);
    }

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

    (compact_palette, indexed)
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
    match caps.protocol {
        TerminalImageProtocol::ITerm2 => {
            let guessed = image::guess_format(bytes).ok();
            let dyn_img = image::load_from_memory(bytes).context("Failed to decode image")?;
            let (width_px, height_px) = (dyn_img.width(), dyn_img.height());
            if width_px == 0 || height_px == 0 {
                bail!("Image dimensions are 0x0");
            }
            let (cols, _) = compute_target_dims(width_px, height_px, max_cols, max_rows);
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
                && width_px <= MAX_PIXEL_WIDTH
                && height_px <= MAX_PIXEL_HEIGHT
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
            let rendered =
                decode_and_render_for_protocol(bytes, max_cols, max_rows, (0, 0, 0), caps)?;
            let mut out = rendered.pixel_payload.unwrap_or_default();
            out.push('\n');
            Ok(out)
        }
        TerminalImageProtocol::Halfblock => {
            let rendered = decode_and_render_halfblocks(bytes, max_cols, max_rows, (0, 0, 0))?;
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

fn fit_within_max_pixels(width_px: u32, height_px: u32) -> (u32, u32) {
    if width_px <= MAX_PIXEL_WIDTH && height_px <= MAX_PIXEL_HEIGHT {
        return (width_px, height_px);
    }
    let scale = (MAX_PIXEL_WIDTH as f64 / width_px as f64)
        .min(MAX_PIXEL_HEIGHT as f64 / height_px as f64);
    let w = ((width_px as f64 * scale).round() as u32).clamp(1, MAX_PIXEL_WIDTH);
    let h = ((height_px as f64 * scale).round() as u32).clamp(1, MAX_PIXEL_HEIGHT);
    (w, h)
}

fn encode_crisp_png(dyn_img: &DynamicImage) -> Result<Vec<u8>> {
    let (w, h) = (dyn_img.width(), dyn_img.height());
    let (enc_w, enc_h) = fit_within_max_pixels(w, h);
    if enc_w == w && enc_h == h {
        encode_png(dyn_img)
    } else {
        let resized = dyn_img.resize_exact(enc_w, enc_h, FilterType::Lanczos3);
        encode_png(&resized)
    }
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
        // Explicit preference overrides auto-detection when TEAMS_IMAGE_PROTOCOL is absent or "auto"
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

        // TEAMS_IMAGE_PROTOCOL overrides everything including TMUX and explicit pref
        let mut env_override = HashMap::new();
        env_override.insert("TMUX", "/tmp/tmux-1000/default,1,0");
        env_override.insert("TEAMS_IMAGE_PROTOCOL", "sixel");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &env_override),
            TerminalImageProtocol::Sixel
        );
        env_override.insert("TEAMS_IMAGE_PROTOCOL", "halfblocks");
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

        // Kitty & Ghostty detection
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

        // WezTerm + WT_SESSION both set -> ITerm2 wins over WT_SESSION
        let mut wez_wt_env = HashMap::new();
        wez_wt_env.insert("WEZTERM_EXECUTABLE", "C:\\Program Files\\WezTerm\\wezterm-gui.exe");
        wez_wt_env.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wez_wt_env),
            TerminalImageProtocol::ITerm2
        );

        // WT_SESSION alone -> Sixel
        let mut wt_only = HashMap::new();
        wt_only.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_only),
            TerminalImageProtocol::Sixel
        );

        // WT_SESSION inside VS Code / Cursor / Windsurf integrated xterm.js -> Halfblock
        let mut wt_vscode = HashMap::new();
        wt_vscode.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        wt_vscode.insert("TERM_PROGRAM", "vscode");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_vscode),
            TerminalImageProtocol::Halfblock
        );

        let mut wt_vscode_pid = HashMap::new();
        wt_vscode_pid.insert("WT_SESSION", "00000000-0000-0000-0000-000000000001");
        wt_vscode_pid.insert("VSCODE_PID", "9876");
        assert_eq!(
            resolve_protocol_from_env_map(ImageProtocolPreference::Auto, &wt_vscode_pid),
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
        let rendered =
            decode_and_render_for_protocol(&solid_png, 40, 20, (0, 0, 0), caps).unwrap();
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
        let grad_rendered =
            decode_and_render_for_protocol(&grad_png, 20, 10, (12, 24, 36), caps).unwrap();
        let grad_payload = grad_rendered.pixel_payload.as_deref().unwrap();
        assert!(grad_payload.starts_with("\x1bP0;1;q\"1;1;"));
        assert!(grad_payload.contains(";2;"));
        assert!(grad_payload.contains('-'));
        assert!(grad_payload.ends_with("\x1b\\"));
    }

    #[test]
    fn test_kitty_and_iterm2_tui_vs_cli_stdout_payloads() {
        let png = make_test_png(16, 8);

        // TUI Kitty payload: fixed image id 4242, q=2, c=..., r=..., C=1, no trailing newline
        let kitty_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Kitty,
            cell_px: (8, 16),
            cell_px_exact: true,
        };
        let kitty_tui =
            decode_and_render_for_protocol(&png, 20, 10, (0, 0, 0), kitty_caps).unwrap();
        let kitty_payload = kitty_tui.pixel_payload.as_deref().unwrap();
        assert!(kitty_payload.starts_with(&format!(
            "\x1b_Gf=100,a=T,q=2,i={KITTY_PREVIEW_IMAGE_ID},c={},r={},C=1,m=",
            kitty_tui.cols, kitty_tui.rows
        )));
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
        let iterm_tui =
            decode_and_render_for_protocol(&png, 20, 10, (0, 0, 0), iterm_caps).unwrap();
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
        let rendered =
            decode_and_render_for_protocol(&jpeg_bytes, 20, 10, (0, 0, 0), caps).unwrap();
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
}
