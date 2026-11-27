//! Terminal image preview (`preview`) commands and attachment selection helpers.

use std::{
    io::{self, Write},
    path::Path,
};

use anyhow::{bail, Context, Result};

use crate::{
    config::{self, AppConfig},
    img_preview,
    models::{AttachmentInfo, MessageItem},
};

use super::{
    conversations::resolve_target,
    history::find_message_with_fallback,
    runtime::open_runtime,
};

pub(crate) fn default_preview_dimensions(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let c = cols.unwrap_or_else(|| term_cols.clamp(20, 120));
    let r = rows.unwrap_or_else(|| term_rows.saturating_sub(4).clamp(10, 40));
    (c, r)
}

pub(super) fn resolve_image_protocol_preference(
    cfg: &AppConfig,
    protocol: Option<&str>,
) -> Result<config::ImageProtocolPreference> {
    match protocol {
        Some(raw) => match config::ImageProtocolPreference::parse_str(raw) {
            Some(pref) => Ok(pref),
            None => bail!(
                "Invalid image protocol '{}'. Valid values: auto, kitty, iterm2, sixel, halfblock",
                raw.trim()
            ),
        },
        None => Ok(cfg.image_protocol),
    }
}

pub(super) fn print_rendered_preview(rendered: &str) -> Result<()> {
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

pub(super) fn cmd_preview_local_file(
    cfg: &AppConfig,
    path: &Path,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<&str>,
) -> Result<()> {
    let effective_pref = resolve_image_protocol_preference(cfg, protocol)?;
    if !path.exists() || !path.is_file() {
        bail!("Image file not found: {}", path.display());
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read image file: {}", path.display()))?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout_with_pref(&bytes, c, r, effective_pref)?;
    print_rendered_preview(&rendered)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_preview_teams(
    cfg: &AppConfig,
    target: Option<&str>,
    msg_id: Option<&str>,
    attachment: Option<&str>,
    thread: Option<&str>,
    file: Option<&Path>,
    cols: Option<u16>,
    rows: Option<u16>,
    protocol: Option<&str>,
) -> Result<()> {
    if let Some(path) = file {
        return cmd_preview_local_file(cfg, path, cols, rows, protocol);
    }
    let effective_pref = resolve_image_protocol_preference(cfg, protocol)?;
    let target = target.context("Specify a target conversation (<target>) or --file <path>.")?;
    let msg_id = msg_id.context("Specify a message ID (<msg_id>) or --file <path>.")?;

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let msg = find_message_with_fallback(&graph, &storage, &conv, msg_id, thread).await?;
    let att = select_teams_attachment(&msg, attachment)?;
    let (_name, _ct, bytes) = graph.fetch_attachment_bytes(&att).await?;
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout_with_pref(&bytes, c, r, effective_pref)?;
    print_rendered_preview(&rendered)
}


pub(crate) fn select_teams_attachment(
    msg: &MessageItem,
    selector: Option<&str>,
) -> Result<AttachmentInfo> {
    let attachments = msg.effective_attachments();
    if attachments.is_empty() {
        bail!("Message has no attachments: {}", msg.msg_id);
    }
    if let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(idx) = sel.parse::<usize>() {
            return attachments.get(idx).cloned().with_context(|| {
                format!(
                    "Attachment index {idx} is out of range (total {})",
                    attachments.len()
                )
            });
        }
        if let Some(found) = attachments.iter().find(|a| {
            a.id == sel
                || a.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(sel))
        }) {
            return Ok(found.clone());
        }
        bail!("Matching attachment not found: {sel}");
    }
    Ok(attachments
        .iter()
        .find(|a| img_preview::is_image_mime_or_name(&a.content_type, a.name.as_deref()))
        .unwrap_or(&attachments[0])
        .clone())
}
