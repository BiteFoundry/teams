//! Bounded raw/rendered terminal image caches and async attachment image decoding.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::img_preview::{self, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol};
use crate::models::AttachmentInfo;

use super::super::overlays::image_preview::PreviewTarget;
use super::super::theme::Theme;
use super::state::{AttachmentDecodeJob, MAX_RAW_IMAGE_CACHE_BYTES, MAX_RAW_IMAGE_CACHE_ENTRIES};
use super::{AppState, UiMsg};

impl AppState {
    pub fn inline_image_bg(theme: &Theme) -> (u8, u8, u8) {
        let fallback = img_preview::color_to_rgb(theme.bg_card, (20, 22, 26));
        img_preview::color_to_rgb(theme.bg_canvas, fallback)
    }

    pub fn modal_image_bg(theme: &Theme) -> (u8, u8, u8) {
        img_preview::color_to_rgb(theme.bg_elevated, (37, 42, 61))
    }

    pub fn image_cache_key(
        owner_id: &str,
        att_id: &str,
        cols: u16,
        rows: u16,
        cell_px: (u16, u16),
        bg_rgb: (u8, u8, u8),
    ) -> String {
        format!(
            "{owner_id}:{att_id}:{cols}x{rows}:{}x{}:{:02x}{:02x}{:02x}",
            cell_px.0, cell_px.1, bg_rgb.0, bg_rgb.1, bg_rgb.2
        )
    }

    pub fn preview_image_cache_key(
        owner_id: &str,
        att_id: &str,
        view_cols: u16,
        view_rows: u16,
        zoom_pct: u16,
        cell_px: (u16, u16),
        protocol: TerminalImageProtocol,
    ) -> String {
        format!(
            "{owner_id}:{att_id}:{view_cols}x{view_rows}:{zoom_pct}%:{}x{}:{}",
            cell_px.0,
            cell_px.1,
            protocol.as_str()
        )
    }

    pub fn raw_image_cache_key(owner_id: &str, att_id: &str) -> String {
        format!("{owner_id}:{att_id}")
    }

    pub fn insert_raw_image_cache(&mut self, raw_key: String, bytes: Arc<[u8]>) {
        let entry_bytes = bytes.len();
        if entry_bytes > MAX_RAW_IMAGE_CACHE_BYTES {
            return;
        }
        if let Some(prev) = self.image_raw_cache.remove(&raw_key) {
            self.image_raw_bytes = self.image_raw_bytes.saturating_sub(prev.len());
            self.image_raw_order.retain(|k| k != &raw_key);
        }
        self.image_raw_cache.insert(raw_key.clone(), bytes);
        self.image_raw_order.push_back(raw_key);
        self.image_raw_bytes = self.image_raw_bytes.saturating_add(entry_bytes);

        while self.image_raw_order.len() > MAX_RAW_IMAGE_CACHE_ENTRIES {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }

        while self.image_raw_bytes > MAX_RAW_IMAGE_CACHE_BYTES && self.image_raw_order.len() > 1 {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }
    }

    fn touch_raw_image_cache(&mut self, raw_key: &str) -> Option<Arc<[u8]>> {
        let bytes = self.image_raw_cache.get(raw_key).cloned()?;
        self.image_raw_order.retain(|k| k != raw_key);
        self.image_raw_order.push_back(raw_key.to_string());
        Some(bytes)
    }

    pub(super) fn rendered_image_bytes(rendered: &RenderedTerminalImage) -> usize {
        rendered.approx_heap_bytes().max(64)
    }

    fn insert_bounded_image_cache(
        cache: &mut HashMap<String, RenderedTerminalImage>,
        order: &mut VecDeque<String>,
        bytes_total: &mut usize,
        max_entries: usize,
        cache_key: String,
        rendered: RenderedTerminalImage,
    ) {
        if let Some(prev) = cache.remove(&cache_key) {
            *bytes_total = bytes_total.saturating_sub(Self::rendered_image_bytes(&prev));
            order.retain(|k| k != &cache_key);
        }
        let entry_bytes = Self::rendered_image_bytes(&rendered);
        cache.insert(cache_key.clone(), rendered);
        order.push_back(cache_key);
        *bytes_total = bytes_total.saturating_add(entry_bytes);

        while order.len() > max_entries {
            let Some(evicted) = order.pop_front() else {
                break;
            };
            if let Some(removed) = cache.remove(&evicted) {
                *bytes_total = bytes_total.saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }

        while *bytes_total > img_preview::MAX_IMAGE_CACHE_BYTES && order.len() > 1 {
            let Some(evicted) = order.pop_front() else {
                break;
            };
            if let Some(removed) = cache.remove(&evicted) {
                *bytes_total = bytes_total.saturating_sub(Self::rendered_image_bytes(&removed));
            }
        }
    }

    pub fn insert_image_cache(&mut self, cache_key: String, rendered: RenderedTerminalImage) {
        const MAX_IMAGE_CACHE: usize = 32;
        self.image_pending.remove(&cache_key);
        self.clear_image_error(&cache_key);
        Self::insert_bounded_image_cache(
            &mut self.image_cache,
            &mut self.image_cache_order,
            &mut self.image_cache_bytes,
            MAX_IMAGE_CACHE,
            cache_key,
            rendered,
        );
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    pub fn insert_image_error(&mut self, cache_key: String, error: String) {
        const MAX_IMAGE_ERRORS: usize = 64;
        if self.image_errors.contains_key(&cache_key) {
            self.image_errors_order.retain(|k| k != &cache_key);
        }
        self.image_errors.insert(cache_key.clone(), error);
        self.image_errors_order.push_back(cache_key);
        while self.image_errors_order.len() > MAX_IMAGE_ERRORS {
            if let Some(evicted) = self.image_errors_order.pop_front() {
                self.image_errors.remove(&evicted);
            }
        }
    }

    pub fn clear_image_error(&mut self, cache_key: &str) {
        if self.image_errors.remove(cache_key).is_some() {
            self.image_errors_order.retain(|k| k != cache_key);
        }
    }

    /// Clears decoded/rendered image entries while preserving raw fetched bytes (e.g. on theme switch).
    pub fn clear_rendered_image_cache(&mut self) {
        self.image_cache.clear();
        self.image_cache_order.clear();
        self.image_cache_bytes = 0;
        self.image_errors.clear();
        self.image_errors_order.clear();
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    pub fn clear_image_cache(&mut self) {
        self.image_cache.clear();
        self.image_cache_order.clear();
        self.image_cache_bytes = 0;
        self.image_errors.clear();
        self.image_errors_order.clear();
        self.image_raw_cache.clear();
        self.image_raw_order.clear();
        self.image_raw_bytes = 0;
        self.image_cache_gen = self.image_cache_gen.wrapping_add(1);
    }

    pub fn clear_image_caches(&mut self) {
        self.clear_image_cache();
    }

    fn spawn_attachment_image_decode(&mut self, job: AttachmentDecodeJob) {
        let AttachmentDecodeJob {
            key,
            msg_id,
            att,
            fit,
            cell_px,
            bg,
            caps,
        } = job;
        if self.image_cache.contains_key(&key)
            || self.image_pending.contains(&key)
            || self.image_errors.contains_key(&key)
        {
            return;
        }
        self.image_pending.insert(key.clone());
        let raw_key = Self::raw_image_cache_key(&msg_id, &att.id);
        let cached_raw = self.touch_raw_image_cache(&raw_key);
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        let att_id = att.id.clone();

        tokio::spawn(async move {
            let raw_bytes = match cached_raw {
                Some(bytes) => Ok(bytes),
                None => match graph.fetch_attachment_bytes(&att).await {
                    Ok((_, _, bytes)) => {
                        let arc: Arc<[u8]> = Arc::from(bytes);
                        let _ = tx.send(UiMsg::ImageBytesFetched {
                            raw_key,
                            bytes: Arc::clone(&arc),
                        });
                        Ok(arc)
                    }
                    Err(err) => Err(err),
                },
            };
            match raw_bytes {
                Ok(bytes) => {
                    let sniffed = bitefoundry_html::sniff_content_kind(
                        Some(att.content_type.as_str()),
                        att.name.as_deref(),
                        Some(&bytes),
                    );
                    if sniffed == bitefoundry_html::ContentKind::Html {
                        let html = String::from_utf8_lossy(&bytes).into_owned();
                        let title = att
                            .name
                            .clone()
                            .unwrap_or_else(|| format!("HTML ({att_id})"));
                        if caps.is_some() {
                            let _ = tx.send(UiMsg::ImageAutoRoutedToHtml {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                title,
                                html,
                            });
                        } else {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: "HTML content detected (press H to view in HTML Preview)"
                                    .to_string(),
                            });
                        }
                        return;
                    }
                    let decode_res = tokio::task::spawn_blocking(move || match caps {
                        Some(c) => img_preview::decode_and_render_for_protocol(&bytes, fit, bg, c),
                        None => img_preview::decode_and_render_halfblocks_with_cell_px(
                            &bytes, fit, bg, cell_px,
                        ),
                    })
                    .await;
                    match decode_res {
                        Ok(Ok(rendered)) => {
                            let _ = tx.send(UiMsg::ImageDecoded {
                                cache_key: key,
                                rendered,
                            });
                        }
                        Ok(Err(err)) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::ImageDecodeFailed {
                                cache_key: key,
                                owner_id: msg_id,
                                att_id,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ImageDecodeFailed {
                        cache_key: key,
                        owner_id: msg_id,
                        att_id,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    pub fn request_teams_image(
        &mut self,
        msg_id: String,
        att: AttachmentInfo,
        cols: u16,
        rows: u16,
        cell_px: (u16, u16),
        bg_rgb: (u8, u8, u8),
    ) {
        let key = Self::image_cache_key(&msg_id, &att.id, cols, rows, cell_px, bg_rgb);
        self.spawn_attachment_image_decode(AttachmentDecodeJob {
            key,
            msg_id,
            att,
            fit: img_preview::FitBox::fixed(cols, rows),
            cell_px,
            bg: bg_rgb,
            caps: None,
        });
    }

    pub fn request_preview_image(
        &mut self,
        target: &PreviewTarget,
        fit: img_preview::FitBox,
        caps: TerminalGraphicsCaps,
        bg_rgb: (u8, u8, u8),
    ) {
        match target {
            PreviewTarget::Teams { msg_id, attachment } => {
                let key = Self::preview_image_cache_key(
                    msg_id,
                    &attachment.id,
                    fit.view_cols,
                    fit.view_rows,
                    fit.zoom_pct,
                    caps.cell_px,
                    caps.protocol,
                );
                self.spawn_attachment_image_decode(AttachmentDecodeJob {
                    key,
                    msg_id: msg_id.clone(),
                    att: attachment.clone(),
                    fit,
                    cell_px: caps.cell_px,
                    bg: bg_rgb,
                    caps: Some(caps),
                });
            }
        }
    }
}
