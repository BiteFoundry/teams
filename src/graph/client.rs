//! Core HTTP transport, resilience (401 refresh-retry, 429/503 Retry-After backoff),
//! `$batch` execution, and shared Graph `chatMessage` wire normalization.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use rand::Rng;
use reqwest::{Method, StatusCode};
use scraper::{Html, Selector};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::GraphClient;
use crate::models::{AttachmentInfo, MentionInfo, MessageItem, ReactionSummary};

const MAX_BATCH_SIZE: usize = 20;
const MAX_THROTTLE_RETRIES: usize = 3;
const MAX_RETRY_AFTER_SECS: u64 = 30;
const STANDARD_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MEDIA_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize)]
pub struct BatchSubRequest {
    pub id: String,
    pub method: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchSubResponse {
    pub id: String,
    pub status: u16,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct BatchResponseEnvelope {
    #[serde(default)]
    responses: Vec<BatchSubResponse>,
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub(crate) struct GraphListPage<T> {
    #[serde(default = "Vec::new")]
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink", default)]
    pub next_link: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub(crate) struct GraphDeltaPage<T> {
    #[serde(default = "Vec::new")]
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink", default)]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink", default)]
    pub delta_link: Option<String>,
}

/// Whether a request may be replayed after a gateway failure.
///
/// `502`/`504` mean an intermediary gave up, not that Graph rejected the request, so a
/// non-idempotent write may already have been applied; replaying it could post twice.
/// `429`/`503` are throttling responses issued before processing and are always safe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryClass {
    /// GET / PUT / PATCH / DELETE and read-only `$batch`: retry 429, 502, 503 and 504.
    Idempotent,
    /// Plain POST writes (send, reply, create chat, createLink…): retry 429 and 503 only.
    NonIdempotent,
}

impl RetryClass {
    fn retries_status(self, status: StatusCode) -> bool {
        match status {
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => true,
            StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT => self == RetryClass::Idempotent,
            _ => false,
        }
    }
}

#[derive(Clone)]
enum RequestBody {
    Empty,
    Json(serde_json::Value),
    Bytes {
        data: Arc<[u8]>,
        content_type: Option<String>,
    },
}

struct ExecuteOptions<'a> {
    extra_headers: &'a [(&'a str, String)],
    use_bearer: bool,
    timeout: Duration,
    retry: RetryClass,
}

impl<'a> ExecuteOptions<'a> {
    fn standard(retry: RetryClass) -> Self {
        Self {
            extra_headers: &[],
            use_bearer: true,
            timeout: STANDARD_REQUEST_TIMEOUT,
            retry,
        }
    }

    fn media(use_bearer: bool) -> Self {
        Self {
            extra_headers: &[],
            use_bearer,
            timeout: MEDIA_REQUEST_TIMEOUT,
            retry: RetryClass::Idempotent,
        }
    }
}

impl GraphClient {
    pub(crate) fn resolve_url(&self, path_or_url: &str) -> String {
        if path_or_url.starts_with("https://") || path_or_url.starts_with("http://") {
            return path_or_url.to_string();
        }
        format!(
            "{}/{}",
            self.config.graph_base_url.trim_end_matches('/'),
            path_or_url.trim_start_matches('/')
        )
    }

    async fn resolve_my_user_id_inner(&self) -> Result<String> {
        if let Some(id) = self.my_user_id.as_deref().filter(|s| !s.trim().is_empty()) {
            return Ok(id.to_string());
        }
        if let Some(id) = self
            .cached_my_user_id
            .get()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(id.to_string());
        }
        let _guard = self.resolve_user_mutex.lock().await;
        if let Some(id) = self
            .cached_my_user_id
            .get()
            .filter(|s| !s.trim().is_empty())
        {
            return Ok(id.to_string());
        }
        if let Some(tok) = self.auth.current_tokens().await {
            if let Some(id) = tok.user_id.filter(|s| !s.trim().is_empty()) {
                let _ = self.cached_my_user_id.set(id.clone());
                return Ok(id);
            }
        }
        let me = self
            .get_me()
            .await
            .context("Failed to determine current user ID (/me).")?;
        if !me.id.trim().is_empty() {
            let _ = self.cached_my_user_id.set(me.id.clone());
        }
        let _ = self
            .auth
            .update_identity(
                me.id.clone(),
                me.display_name.clone(),
                me.mail.clone().or(me.user_principal_name.clone()),
            )
            .await;
        Ok(me.id)
    }

    /// Resolves current user's Entra object ID using cached field, shared OnceLock, stored tokens, or `/me`.
    pub(crate) async fn resolve_my_user_id(&self) -> Option<String> {
        self.resolve_my_user_id_inner().await.ok()
    }

    pub(crate) async fn require_my_user_id(&self) -> Result<String> {
        self.resolve_my_user_id_inner().await
    }

    pub async fn get_json<T: DeserializeOwned>(&self, path_or_url: &str) -> Result<T> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                ExecuteOptions::standard(RetryClass::Idempotent),
            )
            .await?;
        let bytes = resp.bytes().await.context("Failed to read Graph response body")?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to deserialize Graph JSON ({url})"))
    }

    pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        body: &B,
    ) -> Result<T> {
        let url = self.resolve_url(path_or_url);
        let json_val = serde_json::to_value(body)?;
        let resp = self
            .execute_with_retry(
                Method::POST,
                &url,
                RequestBody::Json(json_val),
                ExecuteOptions::standard(RetryClass::NonIdempotent),
            )
            .await?;
        let bytes = resp.bytes().await.context("Failed to read Graph response body")?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to deserialize Graph JSON ({url})"))
    }

    pub async fn post_json_empty<B: Serialize + ?Sized>(
        &self,
        path_or_url: &str,
        body: &B,
    ) -> Result<()> {
        let url = self.resolve_url(path_or_url);
        let json_val = serde_json::to_value(body)?;
        self.execute_with_retry(
            Method::POST,
            &url,
            RequestBody::Json(json_val),
            ExecuteOptions::standard(RetryClass::NonIdempotent),
        )
        .await?;
        Ok(())
    }

    pub async fn patch_json<B: Serialize + ?Sized>(
        &self,
        path_or_url: &str,
        body: &B,
    ) -> Result<()> {
        self.patch_json_with_headers(path_or_url, body, &[]).await?;
        Ok(())
    }

    pub(crate) async fn patch_json_with_headers<B: Serialize + ?Sized>(
        &self,
        path_or_url: &str,
        body: &B,
        headers: &[(&str, String)],
    ) -> Result<reqwest::Response> {
        let url = self.resolve_url(path_or_url);
        let json_val = serde_json::to_value(body)?;
        self.execute_with_retry(
            Method::PATCH,
            &url,
            RequestBody::Json(json_val),
            ExecuteOptions {
                extra_headers: headers,
                ..ExecuteOptions::standard(RetryClass::Idempotent)
            },
        )
        .await
    }

    pub async fn post_empty(&self, path_or_url: &str) -> Result<()> {
        let url = self.resolve_url(path_or_url);
        self.execute_with_retry(
            Method::POST,
            &url,
            RequestBody::Empty,
            ExecuteOptions::standard(RetryClass::NonIdempotent),
        )
        .await?;
        Ok(())
    }

    pub async fn delete_req(&self, path_or_url: &str) -> Result<()> {
        let url = self.resolve_url(path_or_url);
        self.execute_with_retry(
            Method::DELETE,
            &url,
            RequestBody::Empty,
            ExecuteOptions::standard(RetryClass::Idempotent),
        )
        .await?;
        Ok(())
    }

    pub async fn put_bytes<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        bytes: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<T> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::PUT,
                &url,
                RequestBody::Bytes {
                    data: Arc::from(bytes),
                    content_type: content_type.map(str::to_string),
                },
                ExecuteOptions::media(true),
            )
            .await?;
        let resp_bytes = resp.bytes().await.context("Failed to read Graph PUT response body")?;
        serde_json::from_slice(&resp_bytes)
            .with_context(|| format!("Failed to deserialize Graph PUT JSON ({url})"))
    }

    /// Uploads a chunk to a pre-authenticated `createUploadSession` URL without Bearer header.
    pub(crate) async fn put_upload_chunk(
        &self,
        upload_url: &str,
        chunk: &[u8],
        start: u64,
        end_inclusive: u64,
        total: u64,
    ) -> Result<(StatusCode, serde_json::Value)> {
        let headers = vec![
            (
                "Content-Range",
                format!("bytes {start}-{end_inclusive}/{total}"),
            ),
            ("Content-Length", chunk.len().to_string()),
        ];
        let resp = self
            .execute_with_retry(
                Method::PUT,
                upload_url,
                RequestBody::Bytes {
                    data: Arc::from(chunk),
                    content_type: Some("application/octet-stream".to_string()),
                },
                ExecuteOptions {
                    extra_headers: &headers,
                    ..ExecuteOptions::media(false)
                },
            )
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let val = if text.trim().is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
        };
        Ok((status, val))
    }

    pub async fn get_bytes(&self, path_or_url: &str) -> Result<Vec<u8>> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                ExecuteOptions::media(true),
            )
            .await?;
        let bytes = resp.bytes().await.context("Failed to read Graph binary response")?;
        Ok(bytes.to_vec())
    }

    pub(crate) async fn get_bytes_with_limit(
        &self,
        url_or_path: &str,
        use_bearer: bool,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let url = self.resolve_url(url_or_path);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                ExecuteOptions::media(use_bearer),
            )
            .await?;
        if let Some(len) = resp.content_length() {
            if len > max_bytes as u64 {
                bail!(
                    "Response size ({len} bytes) exceeds in-memory limit ({max_bytes} bytes) for {url}"
                );
            }
        }
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.context("Failed to read Graph binary stream chunk")?;
            if out.len().saturating_add(chunk.len()) > max_bytes {
                bail!(
                    "Response body exceeded in-memory limit ({max_bytes} bytes) for {url}"
                );
            }
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }

    /// Streams a Graph or external asset download directly to `dest_path` using a 64 KiB `BufWriter`.
    pub async fn get_to_file(
        &self,
        url_or_path: &str,
        dest_path: &Path,
        use_bearer: bool,
    ) -> Result<u64> {
        let url = self.resolve_url(url_or_path);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                ExecuteOptions::media(use_bearer),
            )
            .await?;

        let file = tokio::fs::File::create(dest_path)
            .await
            .with_context(|| format!("Failed to create destination file: {}", dest_path.display()))?;
        let mut writer = tokio::io::BufWriter::with_capacity(64 * 1024, file);
        let mut stream = resp.bytes_stream();
        let mut total_written: u64 = 0;

        while let Some(chunk_res) = stream.next().await {
            let chunk = match chunk_res {
                Ok(c) => c,
                Err(err) => {
                    let _ = tokio::fs::remove_file(dest_path).await;
                    return Err(err).context("Failed to stream download chunk");
                }
            };
            if let Err(err) = writer.write_all(&chunk).await {
                let _ = tokio::fs::remove_file(dest_path).await;
                return Err(err).with_context(|| {
                    format!("Failed to write downloaded bytes to {}", dest_path.display())
                });
            }
            total_written += chunk.len() as u64;
        }
        if let Err(err) = writer.flush().await {
            let _ = tokio::fs::remove_file(dest_path).await;
            return Err(err).with_context(|| {
                format!("Failed to flush downloaded file {}", dest_path.display())
            });
        }
        Ok(total_written)
    }

    pub async fn get_text(&self, path_or_url: &str) -> Result<String> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::GET,
                &url,
                RequestBody::Empty,
                ExecuteOptions::standard(RetryClass::Idempotent),
            )
            .await?;
        resp.text().await.context("Failed to read Graph text response")
    }

    pub async fn post_bytes_json<T: DeserializeOwned>(
        &self,
        path_or_url: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<T> {
        let url = self.resolve_url(path_or_url);
        let resp = self
            .execute_with_retry(
                Method::POST,
                &url,
                RequestBody::Bytes {
                    data: Arc::from(bytes),
                    content_type: Some(content_type.to_string()),
                },
                ExecuteOptions::standard(RetryClass::NonIdempotent),
            )
            .await?;
        let resp_bytes = resp.bytes().await.context("Failed to read Graph POST response body")?;
        serde_json::from_slice(&resp_bytes)
            .with_context(|| format!("Failed to deserialize Graph POST JSON ({url})"))
    }

    pub async fn patch_bytes_empty(
        &self,
        path_or_url: &str,
        bytes: Vec<u8>,
        content_type: &str,
    ) -> Result<()> {
        let url = self.resolve_url(path_or_url);
        self.execute_with_retry(
            Method::PATCH,
            &url,
            RequestBody::Bytes {
                data: Arc::from(bytes),
                content_type: Some(content_type.to_string()),
            },
            ExecuteOptions::standard(RetryClass::Idempotent),
        )
        .await?;
        Ok(())
    }

    /// Executes `POST /$batch` in chunks of up to 20 sub-requests.
    pub async fn batch(&self, requests: &[BatchSubRequest]) -> Result<Vec<BatchSubResponse>> {
        if requests.is_empty() {
            return Ok(Vec::new());
        }
        // Every sub-request issued today is a GET, which is what makes replaying the whole
        // `$batch` POST after a 502/504 safe. A write sub-request must not ride on this path.
        debug_assert!(
            requests.iter().all(|r| r.method.eq_ignore_ascii_case("GET")),
            "$batch is retried as idempotent; only GET sub-requests are allowed"
        );
        let url = self.resolve_url("/$batch");
        if requests.len() <= MAX_BATCH_SIZE {
            return self.execute_batch_chunk(&url, requests).await;
        }
        const MAX_CONCURRENT_BATCH_CHUNKS: usize = 4;
        let chunks: Vec<Vec<BatchSubRequest>> = requests
            .chunks(MAX_BATCH_SIZE)
            .map(|chunk| chunk.to_vec())
            .collect();
        let client = self.clone();
        let mut chunk_stream = futures_util::stream::iter(chunks)
            .map(|chunk| {
                let client = client.clone();
                let url = url.clone();
                async move { client.execute_batch_chunk(&url, &chunk).await }
            })
            .buffered(MAX_CONCURRENT_BATCH_CHUNKS);
        let mut all = Vec::with_capacity(requests.len());
        while let Some(res) = chunk_stream.next().await {
            all.extend(res?);
        }
        Ok(all)
    }

    async fn execute_batch_chunk(
        &self,
        url: &str,
        chunk: &[BatchSubRequest],
    ) -> Result<Vec<BatchSubResponse>> {
        let payload = serde_json::json!({ "requests": chunk });
        let resp = self
            .execute_with_retry(
                Method::POST,
                url,
                RequestBody::Json(payload),
                ExecuteOptions::standard(RetryClass::Idempotent),
            )
            .await?;
        let bytes = resp.bytes().await.context("Failed to read Graph response body")?;
        let env: BatchResponseEnvelope = serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to deserialize Graph JSON ({url})"))?;
        Ok(env.responses)
    }

    async fn execute_with_retry(
        &self,
        method: Method,
        url: &str,
        body: RequestBody,
        opts: ExecuteOptions<'_>,
    ) -> Result<reqwest::Response> {
        let ExecuteOptions {
            extra_headers,
            use_bearer,
            timeout,
            retry,
        } = opts;
        // One central origin check for every helper, including `@odata.nextLink` values
        // taken from responses or from the SQLite cursor table: the Graph token only ever
        // goes to the configured Graph host (or the hosts `should_send_bearer` allows).
        if use_bearer && !super::media::should_send_bearer(&self.config.graph_base_url, url) {
            let host = url::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_else(|| "<unparseable URL>".to_string());
            bail!("Refusing to send Graph credentials to an unexpected host: {host}");
        }

        let mut unauthorized_retried = false;
        let mut throttle_attempt = 0usize;
        let mut transport_retried = false;

        loop {
            let mut token_used = None;
            let mut req = self.http.request(method.clone(), url).timeout(timeout);

            if use_bearer {
                let token = self.auth.valid_access_token().await?;
                req = req.bearer_auth(&token);
                token_used = Some(token);
            }

            for (name, val) in extra_headers {
                req = req.header(*name, val.as_str());
            }

            match &body {
                RequestBody::Empty => {}
                RequestBody::Json(val) => {
                    req = req.json(val);
                }
                RequestBody::Bytes { data, content_type } => {
                    if let Some(ct) = content_type {
                        req = req.header(reqwest::header::CONTENT_TYPE, ct.as_str());
                    }
                    req = req.body(data.to_vec());
                }
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(err) => {
                    if method == Method::GET
                        && !transport_retried
                        && (err.is_connect() || err.is_timeout())
                    {
                        transport_retried = true;
                        let jitter_ms: u64 = rand::thread_rng().gen_range(0..=250);
                        tokio::time::sleep(Duration::from_millis(100 + jitter_ms)).await;
                        continue;
                    }
                    let context = crate::http::request_context(
                        format!("Graph HTTP request failed ({method} {url})"),
                        &err,
                    );
                    return Err(anyhow::Error::new(err).context(context));
                }
            };
            let status = resp.status();

            if status == StatusCode::UNAUTHORIZED && use_bearer && !unauthorized_retried {
                unauthorized_retried = true;
                if let Some(failed_tok) = token_used.as_deref() {
                    if self.auth.is_refreshable().await {
                        self.auth.refresh_for_failed_token(failed_tok).await?;
                        continue;
                    } else if self.auth.refresh_for_failed_token(failed_tok).await.is_ok() {
                        // External token refresher updated `credentials.json` on disk: retry seamlessly!
                        continue;
                    }
                }
            }

            if retry.retries_status(status) && throttle_attempt < MAX_THROTTLE_RETRIES {
                let delay_secs = parse_retry_after_secs(resp.headers(), throttle_attempt);
                throttle_attempt += 1;
                let jitter_ms: u64 = rand::thread_rng().gen_range(0..=250);
                tokio::time::sleep(
                    Duration::from_secs(delay_secs) + Duration::from_millis(jitter_ms),
                )
                .await;
                continue;
            }

            if !status.is_success() {
                let err_body = resp.text().await.unwrap_or_default();
                let detail = format_graph_error(status, &err_body);
                bail!("{}{method} {url}): {detail}", crate::graph::GRAPH_API_ERROR_PREFIX);
            }

            return Ok(resp);
        }
    }
}

pub(crate) fn parse_retry_after_secs(
    headers: &reqwest::header::HeaderMap,
    attempt: usize,
) -> u64 {
    if let Some(raw) = headers.get(reqwest::header::RETRY_AFTER) {
        if let Ok(s) = raw.to_str() {
            if let Ok(secs) = s.trim().parse::<u64>() {
                return secs.clamp(1, MAX_RETRY_AFTER_SECS);
            }
        }
    }
    match attempt {
        0 => 1,
        1 => 2,
        _ => 4,
    }
}

fn format_graph_error(status: StatusCode, body: &str) -> String {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(err) = val.get("error") {
            let code = err.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let msg = err.get("message").and_then(|v| v.as_str()).unwrap_or("");
            if !code.is_empty() || !msg.is_empty() {
                return format!("{status} [{code}] {msg}");
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        status.to_string()
    } else {
        format!("{status} {trimmed}")
    }
}

// ---------------------------------------------------------------------------
// Shared Graph Wire DTOs & Message Normalization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphIdentity {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphIdentitySet {
    #[serde(default)]
    pub user: Option<GraphIdentity>,
    #[serde(default)]
    pub application: Option<GraphIdentity>,
    #[serde(default)]
    pub device: Option<GraphIdentity>,
}

impl GraphIdentitySet {
    pub fn primary_id(&self) -> Option<&str> {
        self.user
            .as_ref()
            .and_then(|u| u.id.as_deref())
            .or_else(|| self.application.as_ref().and_then(|a| a.id.as_deref()))
            .or_else(|| self.device.as_ref().and_then(|d| d.id.as_deref()))
            .filter(|s| !s.trim().is_empty())
    }

    pub fn primary_name(&self) -> Option<&str> {
        self.user
            .as_ref()
            .and_then(|u| u.display_name.as_deref())
            .or_else(|| {
                self.application
                    .as_ref()
                    .and_then(|a| a.display_name.as_deref())
            })
            .or_else(|| self.device.as_ref().and_then(|d| d.display_name.as_deref()))
            .filter(|s| !s.trim().is_empty())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphItemBody {
    #[serde(rename = "contentType", default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphAttachment {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "contentType", default)]
    pub content_type: Option<String>,
    #[serde(rename = "contentUrl", default)]
    pub content_url: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub content: Option<serde_json::Value>,
    #[serde(rename = "thumbnailUrl", default)]
    pub thumbnail_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphMention {
    #[serde(default)]
    pub id: i32,
    #[serde(rename = "mentionText", default)]
    pub mention_text: Option<String>,
    #[serde(default)]
    pub mentioned: Option<GraphIdentitySet>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphReaction {
    #[serde(rename = "reactionType", default)]
    pub reaction_type: String,
    #[serde(default)]
    pub user: Option<GraphIdentitySet>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct GraphChatMessage {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(rename = "replyToId", default)]
    pub reply_to_id: Option<String>,
    #[serde(rename = "messageType", default)]
    pub message_type: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    pub created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    pub last_modified_date_time: Option<String>,
    #[serde(rename = "deletedDateTime", default)]
    pub deleted_date_time: Option<String>,
    #[serde(rename = "@removed", default)]
    pub removed: Option<serde_json::Value>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub importance: Option<String>,
    #[serde(rename = "webUrl", default)]
    pub web_url: Option<String>,
    #[serde(default)]
    pub from: Option<GraphIdentitySet>,
    #[serde(default)]
    pub body: Option<GraphItemBody>,
    #[serde(default)]
    pub attachments: Vec<GraphAttachment>,
    #[serde(default)]
    pub mentions: Vec<GraphMention>,
    #[serde(default)]
    pub reactions: Vec<GraphReaction>,
    #[serde(default)]
    pub replies: Option<Vec<serde_json::Value>>,
    #[serde(rename = "eventDetail", default)]
    pub event_detail: Option<serde_json::Value>,
}

fn escape_plain_text_to_html_preserving_newlines(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("<br>");
            }
            '\n' => out.push_str("<br>"),
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn normalize_chat_message(
    conv_key: &str,
    parent_msg_id: Option<String>,
    my_user_id: Option<&str>,
    raw: GraphChatMessage,
) -> MessageItem {
    let msg_id = raw.id;
    let msg_key = MessageItem::make_msg_key(conv_key, &msg_id);
    let deleted = raw.deleted_date_time.is_some() || raw.removed.is_some();
    let message_type = raw
        .message_type
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "message".to_string());

    let author_id = raw
        .from
        .as_ref()
        .and_then(|f| f.primary_id())
        .map(str::to_string);
    let author_name = raw
        .from
        .as_ref()
        .and_then(|f| f.primary_name())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if message_type == "systemEventMessage" {
                "System".to_string()
            } else {
                "Unknown".to_string()
            }
        });
    let is_me = match (author_id.as_deref(), my_user_id) {
        (Some(aid), Some(me)) => aid == me,
        _ => false,
    };

    let is_plain_text_body = raw
        .body
        .as_ref()
        .and_then(|b| b.content_type.as_deref())
        .is_some_and(|ct| ct.eq_ignore_ascii_case("text"));
    let raw_body_content = raw
        .body
        .as_ref()
        .and_then(|b| b.content.clone())
        .unwrap_or_default();
    let body_html = if is_plain_text_body
        && (raw_body_content.contains('<') || raw_body_content.contains('&'))
    {
        escape_plain_text_to_html_preserving_newlines(&raw_body_content)
    } else {
        raw_body_content
    };

    let (mut reply_to_id, mut reply_to_author, mut reply_to_preview) =
        extract_quote_from_html(&body_html);

    let mut attachments = Vec::with_capacity(raw.attachments.len());
    for att in raw.attachments {
        let content_type = att.content_type.unwrap_or_default();
        let content_json = att.content.map(|v| match v {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        });

        if content_type == "messageReference" && reply_to_id.is_none() {
            if let Some(ref cj) = content_json {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(cj) {
                    reply_to_id = parsed
                        .get("messageId")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .or_else(|| att.id.clone());
                    reply_to_author = parsed
                        .pointer("/messageSender/user/displayName")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    reply_to_preview = parsed
                        .get("messagePreview")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                }
            }
            if reply_to_id.is_none() {
                reply_to_id = att.id.clone();
            }
        }

        attachments.push(AttachmentInfo {
            id: att.id.unwrap_or_default(),
            content_type,
            content_url: att.content_url,
            name: att.name,
            content_json,
            thumbnail_url: att.thumbnail_url,
        });
    }

    let extra_inline = crate::parser::extract_inline_image_attachments(
        &body_html,
        &attachments,
        conv_key,
        &msg_id,
        parent_msg_id.as_deref(),
    );
    attachments.extend(extra_inline);

    let mentions = raw
        .mentions
        .into_iter()
        .map(|m| {
            let user_id = m
                .mentioned
                .as_ref()
                .and_then(|set| set.primary_id())
                .unwrap_or_default()
                .to_string();
            let display_name = m
                .mention_text
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    m.mentioned
                        .as_ref()
                        .and_then(|set| set.primary_name())
                        .map(str::to_string)
                })
                .unwrap_or_default();
            MentionInfo {
                id: m.id,
                user_id,
                display_name,
            }
        })
        .collect();

    let reactions = aggregate_reactions(&raw.reactions, my_user_id);

    let mut body_plain = crate::parser::html_and_cards_to_plain(&body_html, &attachments);
    if deleted && body_plain.trim().is_empty() {
        body_plain = "(deleted message)".to_string();
    } else if message_type == "systemEventMessage"
        && (body_plain.trim().is_empty() || body_html.trim() == "<systemEventMessage/>")
    {
        if let Some(summary) = format_system_event(raw.event_detail.as_ref()) {
            body_plain = summary;
        } else if body_html.trim() == "<systemEventMessage/>" {
            body_plain.clear();
        }
    }

    let subject = raw.subject.filter(|s| !s.trim().is_empty());
    let reply_count = raw.replies.map(|r| r.len()).unwrap_or(0);

    MessageItem {
        msg_key,
        conv_key: conv_key.to_string(),
        msg_id,
        parent_msg_id,
        reply_to_id,
        reply_to_author,
        reply_to_preview,
        subject,
        author_id,
        author_name,
        is_me,
        created_at: raw.created_date_time.unwrap_or_default(),
        last_modified_at: raw.last_modified_date_time,
        deleted,
        importance: raw
            .importance
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "normal".to_string()),
        message_type,
        body_html,
        body_plain,
        attachments,
        mentions,
        reactions,
        reply_count,
        web_url: raw.web_url,
    }
}

pub(crate) fn aggregate_reactions(
    reactions: &[GraphReaction],
    my_user_id: Option<&str>,
) -> Vec<ReactionSummary> {
    let mut out: Vec<ReactionSummary> = Vec::new();
    let mut idx_map: HashMap<String, usize> = HashMap::new();

    for r in reactions {
        let rt = r.reaction_type.trim();
        if rt.is_empty() {
            continue;
        }
        let uid = r.user.as_ref().and_then(|u| u.primary_id());
        let uname = r
            .user
            .as_ref()
            .and_then(|u| u.primary_name())
            .or(uid)
            .map(str::to_string);
        let is_me = matches!((uid, my_user_id), (Some(u), Some(me)) if u == me);

        if let Some(&pos) = idx_map.get(rt) {
            let entry = &mut out[pos];
            entry.count += 1;
            entry.me_reacted |= is_me;
            if let Some(name) = uname {
                if !entry.users.contains(&name) {
                    entry.users.push(name);
                }
            }
        } else {
            let pos = out.len();
            idx_map.insert(rt.to_string(), pos);
            let users = uname.into_iter().collect();
            out.push(ReactionSummary {
                reaction_type: rt.to_string(),
                count: 1,
                me_reacted: is_me,
                users,
            });
        }
    }

    out
}

pub(crate) fn is_pure_system_noise(msg: &MessageItem) -> bool {
    msg.message_type != "message"
        && !msg.deleted
        && msg.body_plain.trim().is_empty()
        && msg.attachments.is_empty()
}

pub(crate) fn sort_messages_chronologically(messages: &mut [MessageItem]) {
    messages.sort_by(|a, b| {
        let ta = chrono::DateTime::parse_from_rfc3339(&a.created_at)
            .map(|dt| dt.timestamp_nanos_opt().unwrap_or_default());
        let tb = chrono::DateTime::parse_from_rfc3339(&b.created_at)
            .map(|dt| dt.timestamp_nanos_opt().unwrap_or_default());
        match (ta, tb) {
            (Ok(na), Ok(nb)) => na.cmp(&nb).then_with(|| a.msg_id.cmp(&b.msg_id)),
            _ => a
                .created_at
                .cmp(&b.created_at)
                .then_with(|| a.msg_id.cmp(&b.msg_id)),
        }
    });
}

pub(crate) fn truncate_to_newest(messages: &mut Vec<MessageItem>, limit: usize) {
    if limit > 0 && messages.len() > limit {
        let drop_count = messages.len() - limit;
        messages.drain(0..drop_count);
    }
}

fn format_system_event(detail: Option<&serde_json::Value>) -> Option<String> {
    let val = detail?;
    let odata_type = val.get("@odata.type").and_then(|v| v.as_str()).unwrap_or("");
    let suffix = odata_type.rsplit('.').next().unwrap_or(odata_type);
    match suffix {
        "membersAddedEventMessageDetail" => Some("[System] Members added".to_string()),
        "membersDeletedEventMessageDetail" => Some("[System] Members left or were removed".to_string()),
        "chatRenamedEventMessageDetail" => {
            let name = val
                .get("chatDisplayName")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name.is_empty() {
                Some("[System] Chat renamed".to_string())
            } else {
                Some(format!("[System] Chat renamed to '{name}'"))
            }
        }
        "channelRenamedEventMessageDetail" => {
            let name = val
                .get("channelDisplayName")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name.is_empty() {
                Some("[System] Channel renamed".to_string())
            } else {
                Some(format!("[System] Channel renamed to '{name}'"))
            }
        }
        "callStartedEventMessageDetail" => Some("[System] Call/meeting started".to_string()),
        "callEndedEventMessageDetail" => Some("[System] Call/meeting ended".to_string()),
        "teamRenamedEventMessageDetail" => Some("[System] Team renamed".to_string()),
        _ => None,
    }
}

fn extract_quote_from_html(html: &str) -> (Option<String>, Option<String>, Option<String>) {
    if !html.contains("<blockquote") {
        return (None, None, None);
    }
    let frag = Html::parse_fragment(html);
    let Ok(bq_sel) = Selector::parse("blockquote[itemid]") else {
        return (None, None, None);
    };
    let Some(bq) = frag.select(&bq_sel).next() else {
        return (None, None, None);
    };
    let item_id = bq
        .value()
        .attr("itemid")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let author = Selector::parse("strong")
        .ok()
        .and_then(|sel| bq.select(&sel).next())
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty());

    let preview = Selector::parse("p")
        .ok()
        .and_then(|sel| bq.select(&sel).next())
        .map(|el| el.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty());

    (item_id, author, preview)
}

pub(crate) fn compare_rfc3339_gt(lhs: Option<&str>, rhs: Option<&str>) -> bool {
    let Some(l) = lhs.filter(|s| !s.trim().is_empty()) else {
        return false;
    };
    let Some(r) = rhs.filter(|s| !s.trim().is_empty()) else {
        return true;
    };
    match (
        chrono::DateTime::parse_from_rfc3339(l),
        chrono::DateTime::parse_from_rfc3339(r),
    ) {
        (Ok(dl), Ok(dr)) => dl > dr,
        _ => l > r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthManager;
    use crate::config::AppConfig;

    #[test]
    fn test_resolve_url_relative_and_absolute() {
        let cfg = AppConfig {
            graph_base_url: "https://graph.microsoft.com/v1.0/".to_string(),
            ..Default::default()
        };
        let auth = AuthManager::new(cfg.clone()).unwrap_or_else(|_| {
            AuthManager::from_tokens(
                cfg.clone(),
                crate::auth::StoredTokens {
                    access_token: "t".into(),
                    refresh_token: None,
                    expires_at: 9_999_999_999,
                    client_id: "c".into(),
                    tenant_id: "t".into(),
                    user_id: None,
                    user_display_name: None,
                    user_email: None,
                    auth_mode: "access_token".into(),
                },
            )
        });
        let client = GraphClient::new(cfg, auth).unwrap();
        assert_eq!(
            client.resolve_url("/me"),
            "https://graph.microsoft.com/v1.0/me"
        );
        assert_eq!(
            client.resolve_url("me/chats"),
            "https://graph.microsoft.com/v1.0/me/chats"
        );
        assert_eq!(
            client.resolve_url("https://graph.microsoft.com/v1.0/me/chats?$skiptoken=abc"),
            "https://graph.microsoft.com/v1.0/me/chats?$skiptoken=abc"
        );
        assert_eq!(
            client.resolve_url("http://127.0.0.1:8080/next"),
            "http://127.0.0.1:8080/next"
        );
    }

    #[test]
    fn test_normalize_chat_message_and_reactions() {
        let json = serde_json::json!({
            "id": "1700000001",
            "messageType": "message",
            "createdDateTime": "2026-11-15T06:00:00Z",
            "lastModifiedDateTime": "2026-11-15T06:01:00Z",
            "subject": "Weekly Sync",
            "importance": "high",
            "from": {
                "user": { "id": "u-me", "displayName": "Alice" }
            },
            "body": {
                "contentType": "html",
                "content": "<blockquote itemtype=\"http://schema.skype.com/Reply\" itemid=\"1700000000\"><strong>Bob</strong><p>Original message</p></blockquote><p>This is a reply</p>"
            },
            "mentions": [
                {
                    "id": 0,
                    "mentionText": "Bob",
                    "mentioned": { "user": { "id": "u-peer", "displayName": "Bob" } }
                }
            ],
            "reactions": [
                { "reactionType": "like", "user": { "user": { "id": "u-peer", "displayName": "Bob" } } },
                { "reactionType": "like", "user": { "user": { "id": "u-me", "displayName": "Alice" } } },
                { "reactionType": "heart", "user": { "user": { "id": "u-other", "displayName": "Carol" } } }
            ],
            "replies": [{ "id": "r1" }, { "id": "r2" }]
        });

        let raw: GraphChatMessage = serde_json::from_value(json).unwrap();
        let item = normalize_chat_message("chat:c1", None, Some("u-me"), raw);

        assert_eq!(item.msg_key, "chat:c1|1700000001");
        assert!(item.is_me);
        assert_eq!(item.author_name, "Alice");
        assert_eq!(item.subject.as_deref(), Some("Weekly Sync"));
        assert_eq!(item.importance, "high");
        assert_eq!(item.reply_to_id.as_deref(), Some("1700000000"));
        assert_eq!(item.reply_to_author.as_deref(), Some("Bob"));
        assert_eq!(item.reply_to_preview.as_deref(), Some("Original message"));
        assert_eq!(item.reply_count, 2);
        assert_eq!(item.mentions.len(), 1);
        assert_eq!(item.mentions[0].user_id, "u-peer");

        assert_eq!(item.reactions.len(), 2);
        assert_eq!(item.reactions[0].reaction_type, "like");
        assert_eq!(item.reactions[0].count, 2);
        assert!(item.reactions[0].me_reacted);
        assert_eq!(item.reactions[0].users, vec!["Bob", "Alice"]);
        assert_eq!(item.reactions[1].reaction_type, "heart");
        assert_eq!(item.reactions[1].count, 1);
        assert!(!item.reactions[1].me_reacted);
    }

    #[test]
    fn test_normalize_chat_message_synthesizes_inline_hosted_image_attachment() {
        let json = serde_json::json!({
            "id": "m1",
            "messageType": "message",
            "createdDateTime": "2026-11-18T10:00:00Z",
            "from": {
                "user": { "id": "u-peer", "displayName": "Bob" }
            },
            "body": {
                "contentType": "html",
                "content": "<p><img src=\"https://graph.microsoft.com/v1.0/chats/19:abc/messages/m1/hostedContents/aWQ9MQ==/$value\"></p>"
            },
            "attachments": []
        });

        let raw: GraphChatMessage = serde_json::from_value(json).unwrap();
        let item = normalize_chat_message("chat:19:abc", None, Some("u-me"), raw);
        assert!(!is_pure_system_noise(&item));
        assert_eq!(item.attachments.len(), 1);
        assert_eq!(item.attachments[0].id, "hosted:aWQ9MQ==");
        assert_eq!(item.attachments[0].content_type, "hostedContent");
        assert_eq!(
            item.attachments[0].content_url.as_deref(),
            Some("https://graph.microsoft.com/v1.0/chats/19:abc/messages/m1/hostedContents/aWQ9MQ==/$value")
        );
        assert!(crate::img_preview::is_image_mime_or_name(
            &item.attachments[0].content_type,
            item.attachments[0].name.as_deref()
        ));
    }

    #[test]
    fn test_retry_after_header_parsing() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after_secs(&headers, 0), 1);
        assert_eq!(parse_retry_after_secs(&headers, 1), 2);
        assert_eq!(parse_retry_after_secs(&headers, 2), 4);

        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(parse_retry_after_secs(&headers, 0), 7);
    }

    #[tokio::test]
    async fn test_graph_resilience_401_429_and_batch_chunking() {
        use axum::{
            http::{HeaderMap, StatusCode},
            routing::{get, post},
            Json, Router,
        };
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tempfile::tempdir;

        let throttle_calls = Arc::new(AtomicUsize::new(0));
        let throttle_clone = throttle_calls.clone();
        let batch_calls = Arc::new(AtomicUsize::new(0));
        let batch_clone = batch_calls.clone();

        let app = Router::new()
            .route(
                "/tid/oauth2/v2.0/token",
                post(|| async {
                    Json(serde_json::json!({
                        "access_token": "fresh-token",
                        "refresh_token": "rt-2",
                        "expires_in": 3600
                    }))
                }),
            )
            .route(
                "/v1.0/me",
                get(|headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == "Bearer fresh-token" {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "id": "u-1",
                                "displayName": "Alice",
                                "mail": "alice@example.com"
                            })),
                        )
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            Json(serde_json::json!({
                                "error": { "code": "InvalidAuthenticationToken", "message": "expired" }
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/throttled",
                get(move || {
                    let calls = throttle_clone.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        if n == 0 {
                            (
                                StatusCode::TOO_MANY_REQUESTS,
                                [("Retry-After", "1")],
                                Json(serde_json::json!({ "error": { "code": "TooManyRequests" } })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({ "ok": true })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/$batch",
                post(move |Json(body): Json<serde_json::Value>| {
                    let calls = batch_clone.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let reqs = body["requests"].as_array().unwrap();
                        assert!(reqs.len() <= 20);
                        let responses: Vec<serde_json::Value> = reqs
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "id": r["id"],
                                    "status": 200,
                                    "body": { "echo": r["url"] }
                                })
                            })
                            .collect();
                        Json(serde_json::json!({ "responses": responses }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempdir().unwrap();
        let cfg = AppConfig {
            auth_base_url: format!("http://{addr}"),
            graph_base_url: format!("http://{addr}/v1.0"),
            tenant_id: "tid".into(),
            ..Default::default()
        };

        let tok = crate::auth::StoredTokens {
            access_token: "stale-token".into(),
            refresh_token: Some("rt-1".into()),
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".into(),
            tenant_id: "tid".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok)
            .with_token_path(dir.path().join("creds.json"));
        let client = GraphClient::new(cfg, auth).unwrap();

        // 401 -> refresh -> retry succeeds
        let me = client.get_me().await.unwrap();
        assert_eq!(me.id, "u-1");
        assert_eq!(me.display_name, "Alice");

        // 429 -> Retry-After -> retry succeeds
        let val: serde_json::Value = client.get_json("/throttled").await.unwrap();
        assert_eq!(val["ok"], true);
        assert_eq!(throttle_calls.load(Ordering::SeqCst), 2);

        // $batch > 20 chunks into 2 requests
        let sub_reqs: Vec<BatchSubRequest> = (0..25)
            .map(|i| BatchSubRequest {
                id: i.to_string(),
                method: "GET".into(),
                url: format!("/items/{i}"),
                body: None,
                headers: None,
            })
            .collect();
        let batch_resps = client.batch(&sub_reqs).await.unwrap();
        assert_eq!(batch_resps.len(), 25);
        assert_eq!(batch_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_bearer_is_refused_for_hosts_outside_the_graph_origin() {
        let cfg = AppConfig {
            graph_base_url: "https://graph.microsoft.com/v1.0".to_string(),
            ..AppConfig::default()
        };
        let tok = crate::auth::StoredTokens {
            access_token: "secret-token".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("u".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let client = GraphClient::new(cfg.clone(), AuthManager::from_tokens(cfg, tok)).unwrap();

        // Refused before any connection: the error names the host only, never the query.
        for url in [
            "https://evil.example/v1.0/me/chats?$skiptoken=SECRETCURSOR",
            "http://graph.microsoft.com/v1.0/me",
            "https://graph.microsoft.com.evil.example/v1.0/me",
        ] {
            let err = client
                .get_json::<serde_json::Value>(url)
                .await
                .expect_err("credentials must not leave the Graph origin");
            let msg = format!("{err:#}");
            assert!(
                msg.starts_with("Refusing to send Graph credentials to an unexpected host: "),
                "{msg}"
            );
            assert!(!msg.contains("SECRETCURSOR") && !msg.contains("secret-token"), "{msg}");
        }
        let err = client
            .post_json::<_, serde_json::Value>("https://evil.example/collect", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Refusing to send Graph credentials to an unexpected host: evil.example"
        );
    }

    #[test]
    fn test_retry_class_never_replays_writes_after_gateway_errors() {
        for status in [StatusCode::TOO_MANY_REQUESTS, StatusCode::SERVICE_UNAVAILABLE] {
            assert!(RetryClass::Idempotent.retries_status(status));
            assert!(RetryClass::NonIdempotent.retries_status(status));
        }
        for status in [StatusCode::BAD_GATEWAY, StatusCode::GATEWAY_TIMEOUT] {
            assert!(RetryClass::Idempotent.retries_status(status));
            assert!(!RetryClass::NonIdempotent.retries_status(status));
        }
        for status in [StatusCode::INTERNAL_SERVER_ERROR, StatusCode::NOT_FOUND] {
            assert!(!RetryClass::Idempotent.retries_status(status));
            assert!(!RetryClass::NonIdempotent.retries_status(status));
        }
    }

    #[test]
    fn test_retry_after_cap_at_30_secs() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3600".parse().unwrap());
        assert_eq!(parse_retry_after_secs(&headers, 0), 30);

        headers.insert(reqwest::header::RETRY_AFTER, "0".parse().unwrap());
        assert_eq!(parse_retry_after_secs(&headers, 0), 1);
    }

    #[tokio::test]
    async fn test_502_504_retries_and_single_flight_resolve_my_user_id() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let gateway_calls = Arc::new(AtomicUsize::new(0));
        let gw_clone = gateway_calls.clone();
        let me_calls = Arc::new(AtomicUsize::new(0));
        let me_clone = me_calls.clone();

        let app = Router::new()
            .route(
                "/v1.0/gateway-flaky",
                get(move || {
                    let calls = gw_clone.clone();
                    async move {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        match n {
                            0 => (
                                StatusCode::BAD_GATEWAY,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({ "error": { "code": "BadGateway" } })),
                            ),
                            1 => (
                                StatusCode::GATEWAY_TIMEOUT,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({ "error": { "code": "GatewayTimeout" } })),
                            ),
                            _ => (
                                StatusCode::OK,
                                [("Retry-After", "0")],
                                Json(serde_json::json!({ "ok": true })),
                            ),
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me",
                get(move || {
                    let calls = me_clone.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        Json(serde_json::json!({
                            "id": "single-flight-u1",
                            "displayName": "Single Flight",
                            "mail": "sf@example.com"
                        }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..Default::default()
        };
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap();

        let gw_res: serde_json::Value = client.get_json("/gateway-flaky").await.unwrap();
        assert_eq!(gw_res["ok"], true);
        assert_eq!(gateway_calls.load(Ordering::SeqCst), 3);

        let (r1, r2, r3, r4) = tokio::join!(
            client.resolve_my_user_id(),
            client.resolve_my_user_id(),
            client.resolve_my_user_id(),
            client.resolve_my_user_id(),
        );
        assert_eq!(r1.as_deref(), Some("single-flight-u1"));
        assert_eq!(r2.as_deref(), Some("single-flight-u1"));
        assert_eq!(r3.as_deref(), Some("single-flight-u1"));
        assert_eq!(r4.as_deref(), Some("single-flight-u1"));
        assert_eq!(me_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn batch_executes_multiple_chunks_concurrently_and_preserves_order() {
        use axum::{routing::post, Json, Router};
        use tokio::sync::Barrier;

        let barrier = Arc::new(Barrier::new(2));
        let b_clone = Arc::clone(&barrier);

        let app = Router::new().route(
            "/v1.0/$batch",
            post(move |Json(payload): Json<serde_json::Value>| {
                let barrier = Arc::clone(&b_clone);
                async move {
                    // Both chunks MUST be in flight simultaneously or this barrier deadlocks.
                    barrier.wait().await;
                    let reqs = payload["requests"].as_array().cloned().unwrap_or_default();
                    let responses: Vec<serde_json::Value> = reqs
                        .into_iter()
                        .map(|r| {
                            let id = r["id"].as_str().unwrap_or("").to_string();
                            serde_json::json!({
                                "id": id,
                                "status": 200,
                                "body": { "echo": id }
                            })
                        })
                        .collect();
                    Json(serde_json::json!({ "responses": responses }))
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..Default::default()
        };
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
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1");

        let sub_reqs: Vec<BatchSubRequest> = (0..25)
            .map(|i| BatchSubRequest {
                id: i.to_string(),
                method: "GET".to_string(),
                url: format!("/teams/t{i}/channels"),
                body: None,
                headers: None,
            })
            .collect();

        let res = tokio::time::timeout(Duration::from_secs(3), client.batch(&sub_reqs))
            .await
            .expect("batch chunks must overlap concurrently instead of running serially")
            .unwrap();
        let ids: Vec<String> = res.into_iter().map(|r| r.id).collect();
        let expected: Vec<String> = (0..25).map(|i| i.to_string()).collect();
        assert_eq!(ids, expected, "chunk response ordering must be preserved");
        server.abort();
    }
}
