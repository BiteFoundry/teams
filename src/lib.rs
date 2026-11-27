pub mod auth;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod graph;
pub(crate) mod http;
pub mod img_preview;
pub mod mcp;
pub mod models;
pub mod parser;
pub mod resolver;
pub mod safe_text;
pub mod serve;
pub mod storage;
pub mod sync;
pub mod tui;
pub mod ui;
pub mod updater;

/// Encodes a byte slice as a lowercase hexadecimal string.
pub fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}
