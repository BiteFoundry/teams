//! Microsoft Graph v1.0 file upload (OneDrive / SharePoint + chunked upload sessions)
//! and attachment / hosted image download helpers.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use super::GraphClient;
use crate::models::{
    AttachmentInfo, ConversationTarget, DownloadedFile, MessageItem, OutgoingMessage,
    UploadedReferenceAttachment,
};

const SMALL_UPLOAD_LIMIT: u64 = 4 * 1024 * 1024; // 4 MiB
const UPLOAD_CHUNK_SIZE: usize = 3_276_800; // 3.2 MiB (10 * 320 KiB)
const MAX_IN_MEMORY_ATTACHMENT_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

#[derive(Debug, Deserialize)]
struct DriveParentReference {
    #[serde(rename = "driveId", default)]
    drive_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DriveItem {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(rename = "eTag", default)]
    e_tag: Option<String>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(rename = "webDavUrl", default)]
    web_dav_url: Option<String>,
    #[serde(rename = "parentReference", default)]
    parent_reference: Option<DriveParentReference>,
}

#[derive(Debug, Deserialize)]
struct UploadSessionResponse {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
}

#[derive(Debug, Deserialize)]
struct SharingLinkInfo {
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateLinkResponse {
    #[serde(default)]
    link: Option<SharingLinkInfo>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
}

impl GraphClient {
    /// Uploads a local file to the appropriate backing store:
    /// - `Chat` -> `/me/drive/root:/Microsoft Teams Chat Files/{name}:/content` (or upload session > 4 MiB) + sharing link
    /// - `Channel` -> `/teams/{id}/channels/{id}/filesFolder` -> `/drives/{drive_id}/items/{folder_id}:/{name}:/content`
    pub async fn upload_reference_file(
        &self,
        target: &ConversationTarget,
        local_path: &Path,
    ) -> Result<UploadedReferenceAttachment> {
        let meta = tokio::fs::metadata(local_path)
            .await
            .with_context(|| format!("File to upload not found: {}", local_path.display()))?;
        if !meta.is_file() {
            bail!("Only regular files can be uploaded: {}", local_path.display());
        }
        let size = meta.len();
        let file_name = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Invalid file name: {}", local_path.display()))?
            .to_string();
        let encoded_name = urlencoding::encode(&file_name);

        match target {
            ConversationTarget::Chat { .. } => {
                let content_path = chat_upload_content_path(&file_name);
                let session_path = format!(
                    "/me/drive/root:/Microsoft%20Teams%20Chat%20Files/{encoded_name}:/createUploadSession"
                );
                let drive_item = self
                    .upload_drive_item(local_path, &file_name, size, &content_path, &session_path)
                    .await?;

                let att_id = extract_etag_guid(drive_item.e_tag.as_deref());
                let link_path = format!(
                    "/me/drive/items/{}/createLink",
                    urlencoding::encode(&drive_item.id)
                );
                let link_body = serde_json::json!({
                    "type": "view",
                    "scope": "organization"
                });
                let shared_url = match self.post_json::<_, CreateLinkResponse>(&link_path, &link_body).await {
                    Ok(resp) => resp
                        .link
                        .and_then(|l| l.web_url)
                        .or(resp.web_url)
                        .filter(|s| !s.trim().is_empty()),
                    Err(_) => None,
                };

                let content_url = shared_url
                    .or(drive_item.web_dav_url)
                    .or(drive_item.web_url)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Failed to obtain share URL for uploaded file."))?;

                Ok(UploadedReferenceAttachment {
                    id: att_id,
                    name: drive_item.name.unwrap_or(file_name),
                    content_url,
                    size: drive_item.size.unwrap_or(size),
                    drive_item_id: Some(drive_item.id),
                })
            }
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => {
                let folder_path = format!(
                    "/teams/{}/channels/{}/filesFolder",
                    urlencoding::encode(team_id),
                    urlencoding::encode(channel_id)
                );
                let folder: DriveItem = self.get_json(&folder_path).await?;
                let drive_id = folder
                    .parent_reference
                    .and_then(|p| p.drive_id)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Missing driveId on channel filesFolder."))?;

                let content_path = channel_upload_content_path(&drive_id, &folder.id, &file_name);
                let session_path = format!(
                    "/drives/{}/items/{}:/{encoded_name}:/createUploadSession",
                    urlencoding::encode(&drive_id),
                    urlencoding::encode(&folder.id)
                );
                let drive_item = self
                    .upload_drive_item(local_path, &file_name, size, &content_path, &session_path)
                    .await?;

                let att_id = extract_etag_guid(drive_item.e_tag.as_deref());
                let content_url = drive_item
                    .web_dav_url
                    .or(drive_item.web_url)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Missing webDavUrl/webUrl for uploaded channel file."))?;

                Ok(UploadedReferenceAttachment {
                    id: att_id,
                    name: drive_item.name.unwrap_or(file_name),
                    content_url,
                    size: drive_item.size.unwrap_or(size),
                    drive_item_id: Some(drive_item.id),
                })
            }
        }
    }

    async fn upload_drive_item(
        &self,
        local_path: &Path,
        file_name: &str,
        size: u64,
        content_path: &str,
        session_path: &str,
    ) -> Result<DriveItem> {
        if size <= SMALL_UPLOAD_LIMIT {
            let bytes = tokio::fs::read(local_path)
                .await
                .with_context(|| format!("Failed to read file: {}", local_path.display()))?;
            return self
                .put_bytes(content_path, bytes, Some("application/octet-stream"))
                .await;
        }

        let session_req = serde_json::json!({
            "item": {
                "@microsoft.graph.conflictBehavior": "rename",
                "name": file_name
            }
        });
        let session: UploadSessionResponse = self.post_json(session_path, &session_req).await?;

        let mut file = tokio::fs::File::open(local_path)
            .await
            .with_context(|| format!("Failed to open large file: {}", local_path.display()))?;
        let mut offset: u64 = 0;
        let mut buf = vec![0u8; UPLOAD_CHUNK_SIZE];

        loop {
            let mut filled = 0usize;
            while filled < UPLOAD_CHUNK_SIZE {
                let n = file.read(&mut buf[filled..]).await?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                bail!("Upload session reached EOF without a completion response.");
            }

            let end_inclusive = offset + (filled as u64) - 1;
            let (_status, val) = self
                .put_upload_chunk(&session.upload_url, &buf[..filled], offset, end_inclusive, size)
                .await?;
            offset += filled as u64;

            if val.get("id").is_some() {
                let item: DriveItem = serde_json::from_value(val)
                    .context("Failed to deserialize DriveItem on upload session completion")?;
                return Ok(item);
            }
            if offset >= size {
                bail!("Did not receive DriveItem metadata after upload completed.");
            }
        }
    }

    /// High-level helper: uploads `local_path` and sends it with an optional caption (and optional `thread_root_id` in channels).
    pub async fn send_file(
        &self,
        target: &ConversationTarget,
        local_path: &Path,
        caption_html: Option<&str>,
        thread_root_id: Option<&str>,
    ) -> Result<MessageItem> {
        let att = self.upload_reference_file(target, local_path).await?;
        let body_html = match caption_html.map(str::trim).filter(|s| !s.is_empty()) {
            Some(caption) => format!("{caption}<br><attachment id=\"{}\"></attachment>", att.id),
            None => format!("<attachment id=\"{}\"></attachment>", att.id),
        };
        let outgoing = OutgoingMessage {
            body_html,
            attachments: vec![att],
            ..Default::default()
        };
        if let Some(root_id) = thread_root_id.map(str::trim).filter(|s| !s.is_empty()) {
            self.reply_message(target, root_id, &outgoing).await
        } else {
            self.send_message(target, &outgoing).await
        }
    }

    /// Downloads a reference attachment (via Graph `/shares/u!.../driveItem/content` or direct stream)
    /// or hosted inline content into `dest_dir`.
    pub async fn download_attachment(
        &self,
        attachment: &AttachmentInfo,
        dest_dir: &Path,
    ) -> Result<DownloadedFile> {
        let content_url = attachment
            .content_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Attachment has no content_url to download."))?;

        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("Failed to create download directory: {}", dest_dir.display()))?;

        let raw_name = attachment
            .name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| filename_from_url(content_url))
            .unwrap_or_else(|| format!("attachment_{}", attachment.id));

        let safe_name = sanitize_filename(&raw_name);
        let mut dest_path = deduplicate_dest_path(dest_dir, &safe_name);
        let mut final_name = dest_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&safe_name)
            .to_string();

        let is_hosted = content_url.contains("/hostedContents/")
            || content_url.starts_with("hostedContents/")
            || attachment.content_type.eq_ignore_ascii_case("hostedContent");
        let fallback_bearer = should_send_bearer(&self.config.graph_base_url, content_url);

        let bytes_written = if is_hosted {
            self.get_to_file(content_url, &dest_path, fallback_bearer)
                .await?
        } else {
            let share_token = encode_graph_share_token(content_url);
            let share_path = format!("/shares/{share_token}/driveItem/content");
            match self.get_to_file(&share_path, &dest_path, true).await {
                Ok(n) => n,
                Err(err) if is_auth_or_forbidden_error(&err) => return Err(err),
                Err(_) => {
                    self.get_to_file(content_url, &dest_path, fallback_bearer)
                        .await?
                }
            }
        };

        if is_hosted {
            if let Some((new_path, new_name)) =
                maybe_refine_hosted_image_extension(&dest_path, dest_dir, &safe_name, &final_name)
                    .await
            {
                dest_path = new_path;
                final_name = new_name;
            }
        }

        let abs_path = if dest_path.is_absolute() {
            dest_path
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(&dest_path)
        } else {
            dest_path
        };

        let (mime_type, is_image) =
            detect_downloaded_mime_and_image(&abs_path, &final_name, &attachment.content_type)
                .await;

        Ok(DownloadedFile {
            path: abs_path,
            file_name: final_name,
            bytes_written,
            mime_type,
            is_image,
        })
    }

    pub async fn download_hosted_bytes(&self, hosted_url: &str) -> Result<Vec<u8>> {
        if let Some((_, bytes)) = try_decode_data_image_url(hosted_url)? {
            return Ok(bytes);
        }
        let use_bearer = should_send_bearer(&self.config.graph_base_url, hosted_url);
        self.get_bytes_with_limit(hosted_url, use_bearer, MAX_IN_MEMORY_ATTACHMENT_BYTES)
            .await
    }

    /// Downloads raw attachment bytes in memory (for inline terminal image preview) along with `(file_name, content_type, bytes)`.
    pub async fn fetch_attachment_bytes(
        &self,
        attachment: &AttachmentInfo,
    ) -> Result<(String, String, Vec<u8>)> {
        let content_url = attachment
            .content_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Attachment has no content_url to download."))?;
        if let Some((mime, bytes)) = try_decode_data_image_url(content_url)? {
            let raw_name = attachment
                .name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    let ext = match mime.as_str() {
                        "image/jpeg" | "image/jpg" => "jpg",
                        "image/gif" => "gif",
                        "image/webp" => "webp",
                        "image/bmp" | "image/x-ms-bmp" => "bmp",
                        _ => "png",
                    };
                    format!("attachment_{}.{ext}", attachment.id)
                });
            return Ok((raw_name, mime, bytes));
        }
        let raw_name = attachment
            .name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .or_else(|| filename_from_url(content_url))
            .unwrap_or_else(|| format!("attachment_{}", attachment.id));
        let is_hosted = content_url.contains("/hostedContents/")
            || content_url.starts_with("hostedContents/")
            || attachment.content_type.eq_ignore_ascii_case("hostedContent");
        let fallback_bearer = should_send_bearer(&self.config.graph_base_url, content_url);
        let bytes = if is_hosted {
            self.get_bytes_with_limit(content_url, fallback_bearer, MAX_IN_MEMORY_ATTACHMENT_BYTES)
                .await?
        } else {
            let share_token = encode_graph_share_token(content_url);
            let share_path = format!("/shares/{share_token}/driveItem/content");
            match self
                .get_bytes_with_limit(&share_path, true, MAX_IN_MEMORY_ATTACHMENT_BYTES)
                .await
            {
                Ok(b) => b,
                Err(err) if is_auth_or_forbidden_error(&err) => return Err(err),
                Err(_) => {
                    self.get_bytes_with_limit(
                        content_url,
                        fallback_bearer,
                        MAX_IN_MEMORY_ATTACHMENT_BYTES,
                    )
                    .await?
                }
            }
        };
        Ok((raw_name, attachment.content_type.clone(), bytes))
    }
}

/// Decodes an inline `data:image/*` URI locally (rasterizing SVG to PNG) without issuing any HTTP request.
/// Returns `Ok(None)` when `url` is not a `data:image/` URI.
fn try_decode_data_image_url(url: &str) -> Result<Option<(String, Vec<u8>)>> {
    let trimmed = url.trim();
    if trimmed.len() < 11 || !trimmed[..11].eq_ignore_ascii_case("data:image/") {
        return Ok(None);
    }
    let decoded = bitefoundry_html::graphics::decode_data_image_uri(trimmed)?;
    if decoded.is_svg {
        let raster = bitefoundry_html::graphics::rasterize_svg_to_png(
            &decoded.bytes,
            bitefoundry_html::graphics::MAX_RASTER_DIM_PX,
            bitefoundry_html::graphics::MAX_RASTER_DIM_PX,
            None,
        )?;
        Ok(Some(("image/png".to_string(), raster.png_bytes.to_vec())))
    } else {
        Ok(Some((decoded.mime, decoded.bytes)))
    }
}

/// Query appended to small-file `PUT …:/content` uploads so a second upload of the same
/// name creates a new driveItem instead of silently replacing the earlier file (the
/// large-file upload session already sends `"rename"` in its body).
const UPLOAD_CONFLICT_QUERY: &str = "?@microsoft.graph.conflictBehavior=rename";

pub(crate) fn chat_upload_content_path(file_name: &str) -> String {
    format!(
        "/me/drive/root:/Microsoft%20Teams%20Chat%20Files/{}:/content{UPLOAD_CONFLICT_QUERY}",
        urlencoding::encode(file_name)
    )
}

pub(crate) fn channel_upload_content_path(
    drive_id: &str,
    folder_id: &str,
    file_name: &str,
) -> String {
    format!(
        "/drives/{}/items/{}:/{}:/content{UPLOAD_CONFLICT_QUERY}",
        urlencoding::encode(drive_id),
        urlencoding::encode(folder_id),
        urlencoding::encode(file_name)
    )
}

pub(crate) fn should_send_bearer(graph_base_url: &str, target_url: &str) -> bool {
    let trimmed = target_url.trim();
    if trimmed.starts_with('/') && !trimmed.starts_with("//") {
        return true;
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return !trimmed.starts_with("//") && !trimmed.contains("://") && !trimmed.is_empty();
    }
    let Ok(target) = url::Url::parse(trimmed) else {
        return false;
    };
    let Some(target_host) = target.host_str() else {
        return false;
    };
    if let Ok(base) = url::Url::parse(graph_base_url.trim()) {
        if let Some(base_host) = base.host_str() {
            if target.scheme() == base.scheme()
                && target.port_or_known_default() == base.port_or_known_default()
            {
                if target_host.eq_ignore_ascii_case(base_host) {
                    return true;
                }
                let is_loopback = |h: &str| {
                    h.eq_ignore_ascii_case("127.0.0.1") || h.eq_ignore_ascii_case("localhost")
                };
                if is_loopback(base_host) && is_loopback(target_host) {
                    return true;
                }
            }
        }
    }
    if target.scheme() == "https" {
        let lower = target_host.to_ascii_lowercase();
        if lower == "microsoft.com" || lower.ends_with(".microsoft.com") {
            return true;
        }
    }
    false
}

fn is_auth_or_forbidden_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    let detail = msg
        .strip_prefix(crate::graph::GRAPH_API_ERROR_PREFIX)
        .and_then(|rest| rest.split_once("): ").map(|(_, d)| d))
        .unwrap_or(&msg);
    let lower = detail.to_ascii_lowercase();
    detail.starts_with("401 ")
        || detail.starts_with("403 ")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient privileges")
}

/// Extracts a lowercase GUID from a DriveItem `eTag` (e.g. `"{153FA47D-18C9-4179-BE08-9879815A9F90},1"`),
/// falling back to a fresh random UUID v4 when absent or unparseable.
pub(crate) fn extract_etag_guid(etag: Option<&str>) -> String {
    if let Some(raw) = etag {
        // Scan candidate 36-char UUID windows inside the eTag string
        let bytes = raw.as_bytes();
        if bytes.len() >= 36 {
            for start in 0..=bytes.len() - 36 {
                let candidate = &raw[start..start + 36];
                if let Ok(parsed) = Uuid::parse_str(candidate) {
                    return parsed.hyphenated().to_string();
                }
            }
        }
    }
    Uuid::new_v4().hyphenated().to_string()
}

/// Encodes a sharing/WebDAV URL into a Microsoft Graph `/shares/{shareId}` token (`"u!" + base64url_no_pad`).
pub(crate) fn encode_graph_share_token(url: &str) -> String {
    format!("u!{}", URL_SAFE_NO_PAD.encode(url.as_bytes()))
}

fn filename_from_url(raw_url: &str) -> Option<String> {
    let parsed = url::Url::parse(raw_url).ok()?;
    let seg = parsed.path_segments()?.rfind(|s| !s.is_empty())?;
    let decoded = urlencoding::decode(seg).ok()?.into_owned();
    let trimmed = decoded.trim().to_string();
    if trimmed.is_empty() || trimmed == "$value" {
        None
    } else {
        Some(trimmed)
    }
}

pub(crate) fn sanitize_filename(raw: &str) -> String {
    let leaf = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);
    let mut cleaned: String = leaf
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "attachment".to_string()
    } else {
        cleaned
    }
}

pub(crate) fn deduplicate_dest_path(dest_dir: &Path, safe_name: &str) -> PathBuf {
    let candidate = dest_dir.join(safe_name);
    if !candidate.exists() {
        return candidate;
    }

    let p = Path::new(safe_name);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("file");
    let ext = p.extension().and_then(|s| s.to_str());

    for idx in 1..10_000 {
        let next_name = match ext {
            Some(e) if !e.is_empty() => format!("{stem} ({idx}).{e}"),
            _ => format!("{stem} ({idx})"),
        };
        let next_path = dest_dir.join(&next_name);
        if !next_path.exists() {
            return next_path;
        }
    }
    dest_dir.join(format!("{stem}_{}", Uuid::new_v4()))
}

async fn maybe_refine_hosted_image_extension(
    dest_path: &Path,
    dest_dir: &Path,
    safe_name: &str,
    final_name: &str,
) -> Option<(PathBuf, String)> {
    let cur_ext = Path::new(safe_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let is_generic = cur_ext.is_empty()
        || safe_name.starts_with("inline-image-")
        || safe_name.starts_with("attachment_");
    if !is_generic {
        return None;
    }

    let mut header = [0u8; 16];
    let n = {
        let mut f = tokio::fs::File::open(dest_path).await.ok()?;
        let mut read = 0usize;
        while read < header.len() {
            let chunk = f.read(&mut header[read..]).await.ok()?;
            if chunk == 0 {
                break;
            }
            read += chunk;
        }
        read
    };
    if n < 3 {
        return None;
    }

    let detected_ext = match image::guess_format(&header[..n]).ok()? {
        image::ImageFormat::Png => "png",
        image::ImageFormat::Jpeg => "jpg",
        image::ImageFormat::Gif => "gif",
        image::ImageFormat::WebP => "webp",
        _ => return None,
    };

    if cur_ext.eq_ignore_ascii_case(detected_ext)
        || (cur_ext.eq_ignore_ascii_case("jpeg") && detected_ext == "jpg")
    {
        return None;
    }

    let candidate_path = Path::new(final_name).with_extension(detected_ext);
    let candidate_name = candidate_path.file_name()?.to_str()?;
    let target_path = deduplicate_dest_path(dest_dir, candidate_name);
    tokio::fs::rename(dest_path, &target_path).await.ok()?;
    let target_name = target_path
        .file_name()
        .and_then(|s| s.to_str())?
        .to_string();
    Some((target_path, target_name))
}

async fn detect_downloaded_mime_and_image(
    path: &Path,
    file_name: &str,
    declared_content_type: &str,
) -> (String, bool) {
    let mut header = [0u8; 512];
    let n = match tokio::fs::File::open(path).await {
        Ok(mut f) => {
            let mut read = 0usize;
            while read < header.len() {
                match f.read(&mut header[read..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(chunk) => read += chunk,
                }
            }
            read
        }
        Err(_) => 0,
    };

    if n >= 3 {
        if let Ok(fmt) = image::guess_format(&header[..n]) {
            let mime = match fmt {
                image::ImageFormat::Png => Some("image/png"),
                image::ImageFormat::Jpeg => Some("image/jpeg"),
                image::ImageFormat::Gif => Some("image/gif"),
                image::ImageFormat::WebP => Some("image/webp"),
                _ => None,
            };
            if let Some(m) = mime {
                return (m.to_string(), true);
            }
        }
        if crate::img_preview::looks_like_svg_bytes(&header[..n]) {
            return ("image/svg+xml".to_string(), true);
        }
        if crate::img_preview::looks_like_html_bytes(&header[..n]) {
            return ("text/html".to_string(), false);
        }
    }

    let declared = declared_content_type.trim().to_ascii_lowercase();
    let mime = if declared.contains('/')
        && !declared.eq_ignore_ascii_case("application/octet-stream")
    {
        declared
    } else {
        mime_from_extension(file_name).to_string()
    };

    let is_image = crate::img_preview::is_image_mime_or_name(&mime, Some(file_name));
    (mime, is_image)
}

pub(crate) fn mime_from_extension(file_name: &str) -> &'static str {
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("bmp") => "image/bmp",
        Some("pdf") => "application/pdf",
        Some("txt" | "log") => "text/plain",
        Some("md" | "markdown") => "text/markdown",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        Some("xml") => "application/xml",
        Some("zip") => "application/zip",
        Some("gz" | "tgz") => "application/gzip",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("mp4") => "video/mp4",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_extract_etag_guid_variants() {
        assert_eq!(
            extract_etag_guid(Some("\"{153FA47D-18C9-4179-BE08-9879815A9F90},1\"")),
            "153fa47d-18c9-4179-be08-9879815a9f90"
        );
        assert_eq!(
            extract_etag_guid(Some("{153FA47D-18C9-4179-BE08-9879815A9F90},42")),
            "153fa47d-18c9-4179-be08-9879815a9f90"
        );
        assert_eq!(
            extract_etag_guid(Some("W/\"153fa47d-18c9-4179-be08-9879815a9f90\"")),
            "153fa47d-18c9-4179-be08-9879815a9f90"
        );
        let fallback = extract_etag_guid(None);
        assert!(Uuid::parse_str(&fallback).is_ok());
    }

    #[test]
    fn test_encode_graph_share_token() {
        let url = "https://contoso.sharepoint.com/sites/eng/Shared Documents/spec.pdf";
        let token = encode_graph_share_token(url);
        assert!(token.starts_with("u!"));
        assert!(!token.contains('='));
        assert!(!token.contains('+'));
        assert!(!token.contains('/'));
        let decoded = URL_SAFE_NO_PAD.decode(&token[2..]).unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), url);
    }

    #[test]
    fn test_sanitize_and_deduplicate_filename() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("re:po*rt?.pdf"), "re_po_rt_.pdf");
        assert_eq!(sanitize_filename(".."), "attachment");

        let dir = tempdir().unwrap();
        let first = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(first.file_name().unwrap(), "report.pdf");
        std::fs::write(&first, b"1").unwrap();

        let second = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(second.file_name().unwrap(), "report (1).pdf");
        std::fs::write(&second, b"2").unwrap();

        let third = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(third.file_name().unwrap(), "report (2).pdf");
    }

    #[test]
    fn test_small_upload_content_paths_request_rename_on_conflict() {
        const SUFFIX: &str = ":/content?@microsoft.graph.conflictBehavior=rename";
        for name in ["report.pdf", "Q3 report #2.pdf", "보고서 최종.pdf"] {
            let chat = chat_upload_content_path(name);
            assert!(chat.ends_with(SUFFIX), "chat path: {chat}");
            assert!(chat.starts_with("/me/drive/root:/Microsoft%20Teams%20Chat%20Files/"));
            let channel = channel_upload_content_path("b!drive", "folder-1", name);
            assert!(channel.ends_with(SUFFIX), "channel path: {channel}");
            assert!(channel.starts_with("/drives/b%21drive/items/folder-1:/"));
            // The file name itself never leaks a raw '#', '?' or space into the path.
            for path in [&chat, &channel] {
                let before_query = path.split('?').next().unwrap();
                assert!(!before_query.contains('#') && !before_query.contains(' '));
                assert_eq!(path.matches('?').count(), 1);
            }
        }
        assert_eq!(
            chat_upload_content_path("a b.txt"),
            "/me/drive/root:/Microsoft%20Teams%20Chat%20Files/a%20b.txt:/content?@microsoft.graph.conflictBehavior=rename"
        );
    }

    #[test]
    fn test_should_send_bearer_origin_guard() {
        let base = "https://graph.microsoft.com/v1.0";
        assert!(should_send_bearer(base, "/me/drive/items/1/content"));
        assert!(should_send_bearer(
            base,
            "/chats/c1/messages/m1/hostedContents/1/$value"
        ));
        assert!(should_send_bearer(base, "https://graph.microsoft.com/v1.0/shares/123"));
        assert!(should_send_bearer(base, "https://cdn.microsoft.com/file.bin"));
        assert!(!should_send_bearer(base, "https://evil.example.com/steal"));
        assert!(!should_send_bearer(base, "https://evil.example.com/x.png"));
        assert!(!should_send_bearer(base, "//evil.example.com/x.png"));
        assert!(!should_send_bearer(base, "https://contoso.sharepoint.com/sites/a/doc.pdf"));
        assert!(!should_send_bearer(base, "http://graph.microsoft.com/v1.0/me"));

        let mock_base = "http://127.0.0.1:4567/v1.0";
        assert!(should_send_bearer(mock_base, "http://127.0.0.1:4567/files/a.bin"));
        assert!(should_send_bearer(mock_base, "http://localhost:4567/files/a.bin"));
        assert!(!should_send_bearer(mock_base, "http://127.0.0.1:9999/files/a.bin"));
    }

    #[tokio::test]
    async fn test_download_attachment_streams_and_protects_bearer_and_403() {
        use axum::{
            http::{header, HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        use std::sync::{Arc, Mutex};

        let saw_ext_auth = Arc::new(Mutex::new(Option::<String>::None));
        let saw_ext_clone = saw_ext_auth.clone();

        let ext_app = Router::new().route(
            "/external/file.txt",
            get(move |headers: HeaderMap| {
                let saw = saw_ext_clone.clone();
                async move {
                    *saw.lock().unwrap() = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    (StatusCode::OK, b"external-payload-ok".to_vec())
                }
            }),
        );
        let ext_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ext_addr = ext_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(ext_listener, ext_app).await;
        });

        let forbidden_url = format!(
            "http://127.0.0.1:{}/external/file.txt?forbidden=1",
            ext_addr.port()
        );
        let forbidden_share_tok = encode_graph_share_token(&forbidden_url);
        let graph_app = Router::new().route(
            "/v1.0/shares/{token}/driveItem/content",
            get(move |axum::extract::Path(token): axum::extract::Path<String>| {
                let forbidden_tok = forbidden_share_tok.clone();
                async move {
                    if token == forbidden_tok {
                        (StatusCode::FORBIDDEN, "forbidden")
                    } else {
                        (StatusCode::NOT_FOUND, "not found")
                    }
                }
            }),
        );
        let graph_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let graph_addr = graph_listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(graph_listener, graph_app).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = format!("http://{graph_addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "secret-graph-bearer".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let dir = tempdir().unwrap();

        // 1. 404 on /shares falls back to external content_url WITHOUT leaking Bearer token
        let ext_url = format!("http://127.0.0.1:{}/external/file.txt", ext_addr.port());
        let att = AttachmentInfo {
            id: "a1".into(),
            content_type: "reference".into(),
            content_url: Some(ext_url),
            name: Some("ext.txt".into()),
            content_json: None,
            thumbnail_url: None,
        };
        let dl = client.download_attachment(&att, dir.path()).await.unwrap();
        assert!(dl.path.is_absolute());
        assert_eq!(std::fs::read(&dl.path).unwrap(), b"external-payload-ok");
        assert_eq!(dl.bytes_written, 19);
        assert_eq!(dl.mime_type, "text/plain");
        assert!(!dl.is_image);
        assert_eq!(*saw_ext_auth.lock().unwrap(), None);

        // 2. 403 on /shares must NOT be swallowed or fall back to content_url
        let forbidden_att = AttachmentInfo {
            id: "a2".into(),
            content_type: "reference".into(),
            content_url: Some(forbidden_url),
            name: Some("forbidden.txt".into()),
            content_json: None,
            thumbnail_url: None,
        };
        let err = client
            .download_attachment(&forbidden_att, dir.path())
            .await
            .unwrap_err();
        assert!(is_auth_or_forbidden_error(&err));
        assert!(!dir.path().join("forbidden.txt").exists());
    }

    #[tokio::test]
    async fn test_get_bytes_with_limit_rejects_oversized_response() {
        use axum::{routing::get, Router};

        let app = Router::new().route(
            "/v1.0/big",
            get(|| async { vec![b'x'; 2048] }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let err = client
            .get_bytes_with_limit("/big", true, 1024)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exceeds in-memory limit"));
    }

    #[tokio::test]
    async fn test_download_hosted_image_sends_bearer_and_sniffs_jpeg_extension() {
        use axum::{
            http::{header, HeaderMap, StatusCode},
            routing::get,
            Router,
        };
        use std::sync::{Arc, Mutex};

        let saw_auth = Arc::new(Mutex::new(Option::<String>::None));
        let saw_auth_clone = saw_auth.clone();
        // Minimal JPEG header + dummy bytes
        let jpeg_bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01];
        let jpeg_clone = jpeg_bytes.clone();

        let app = Router::new().route(
            "/v1.0/chats/c1/messages/m1/hostedContents/1/$value",
            get(move |headers: HeaderMap| {
                let saw = saw_auth_clone.clone();
                let payload = jpeg_clone.clone();
                async move {
                    *saw.lock().unwrap() = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    (StatusCode::OK, payload)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "hosted-secret-token".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let dir = tempdir().unwrap();
        let att = AttachmentInfo {
            id: "hosted:1".into(),
            content_type: "hostedContent".into(),
            content_url: Some("/chats/c1/messages/m1/hostedContents/1/$value".into()),
            name: Some("inline-image-1.png".into()),
            content_json: None,
            thumbnail_url: None,
        };
        let dl = client.download_attachment(&att, dir.path()).await.unwrap();
        assert_eq!(dl.file_name, "inline-image-1.jpg");
        assert!(dl.path.ends_with("inline-image-1.jpg"));
        assert_eq!(std::fs::read(&dl.path).unwrap(), jpeg_bytes);
        assert_eq!(
            saw_auth.lock().unwrap().as_deref(),
            Some("Bearer hosted-secret-token")
        );
    }

    #[tokio::test]
    async fn test_detect_downloaded_mime_supports_svg_and_rejects_html_spoof() {
        let dir = tempdir().unwrap();

        let svg_path = dir.path().join("diagram.svg");
        std::fs::write(
            &svg_path,
            b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"></svg>",
        )
        .unwrap();
        let (mime, is_image) =
            detect_downloaded_mime_and_image(&svg_path, "diagram.svg", "image/svg+xml").await;
        assert_eq!(mime, "image/svg+xml");
        assert!(is_image, "SVG is now natively supported as a previewable image");

        // Empty/unknown body with declared image/svg+xml is also classified as SVG image
        let empty_svg = dir.path().join("empty.svg");
        std::fs::write(&empty_svg, b"").unwrap();
        let (mime2, is_image2) =
            detect_downloaded_mime_and_image(&empty_svg, "empty.svg", "image/svg+xml").await;
        assert_eq!(mime2, "image/svg+xml");
        assert!(is_image2);

        // HTML auth redirect disguised as a .png file must still be classified as text/html, not image
        let html_png = dir.path().join("login.png");
        std::fs::write(
            &html_png,
            b"<!DOCTYPE html><html><body>Sign in</body></html>",
        )
        .unwrap();
        let (mime3, is_image3) =
            detect_downloaded_mime_and_image(&html_png, "login.png", "image/png").await;
        assert_eq!(mime3, "text/html");
        assert!(!is_image3);

        // Real PNG header is classified as image/png + is_image = true
        let real_png = dir.path().join("ok.bin");
        std::fs::write(
            &real_png,
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13],
        )
        .unwrap();
        let (mime4, is_image4) =
            detect_downloaded_mime_and_image(&real_png, "ok.bin", "application/octet-stream").await;
        assert_eq!(mime4, "image/png");
        assert!(is_image4);
    }

    #[tokio::test]
    async fn test_fetch_attachment_bytes_decodes_data_image_uri_locally_without_network() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
        use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
        use std::io::Cursor;

        // Point graph_base_url at an unreachable local port so any network call would fail immediately.
        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = "http://127.0.0.1:1/v1.0".to_string();
        let tok = crate::auth::StoredTokens {
            access_token: "unused".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let img = ImageBuffer::from_pixel(4, 4, Rgba([220u8, 40, 60, 255]));
        let mut png_buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut png_buf, ImageFormat::Png)
            .unwrap();
        let png_bytes = png_buf.into_inner();
        let data_uri = format!("data:image/png;base64,{}", B64.encode(&png_bytes));

        let att = AttachmentInfo {
            id: "inline-1".into(),
            content_type: "image/png".into(),
            content_url: Some(data_uri.clone()),
            name: None,
            content_json: None,
            thumbnail_url: None,
        };
        let (name, mime, bytes) = client.fetch_attachment_bytes(&att).await.unwrap();
        assert_eq!(name, "attachment_inline-1.png");
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, png_bytes);

        let hosted = client.download_hosted_bytes(&data_uri).await.unwrap();
        assert_eq!(hosted, png_bytes);

        // Percent-encoded SVG data URI is also decoded and rasterized to PNG locally.
        let svg_uri = "data:image/svg+xml,%3Csvg%20width%3D%2216%22%20height%3D%2216%22%3E%3Crect%20width%3D%2216%22%20height%3D%2216%22%20fill%3D%22%23e11d48%22/%3E%3C/svg%3E";
        let svg_att = AttachmentInfo {
            id: "inline-svg".into(),
            content_type: "image/svg+xml".into(),
            content_url: Some(svg_uri.into()),
            name: Some("chart.svg".into()),
            content_json: None,
            thumbnail_url: None,
        };
        let (svg_name, svg_mime, svg_png) = client.fetch_attachment_bytes(&svg_att).await.unwrap();
        assert_eq!(svg_name, "chart.svg");
        assert_eq!(svg_mime, "image/png");
        assert!(svg_png.starts_with(b"\x89PNG"));
    }
}
