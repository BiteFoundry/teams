//! Application configuration, local paths, settings (`config.toml`), and per-conversation/thread drafts (`drafts.json`).
//!
//! Shared foundation module: treat public signatures here as stable across all modules.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const APP_DIR_NAME: &str = "teams-cli";
pub const DEFAULT_GRAPH_BASE_URL: &str = "https://graph.microsoft.com/v1.0";
pub const DEFAULT_AUTH_BASE_URL: &str = "https://login.microsoftonline.com";
/// Default Microsoft multi-tenant public client ID fallback when user does not supply a custom Azure App Client ID.
/// Users can always override via `--client-id`, `TEAMS_CLIENT_ID`, `AZURE_CLIENT_ID`, or `config.toml`.
pub const DEFAULT_PUBLIC_CLIENT_ID: &str = "14d82eec-204b-4c2f-b7e8-296a70dab67e";
pub const DEFAULT_TENANT_ID: &str = "common";
pub const DEFAULT_GITHUB_REPO: &str = "BiteFoundry/teams";

/// Message stream density mode in TUI (`Cozy` grouped default vs `Compact` single-line gutter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DensityMode {
    #[default]
    Cozy,
    Compact,
}

impl DensityMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Cozy => Self::Compact,
            Self::Compact => Self::Cozy,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cozy => "Cozy",
            Self::Compact => "Compact",
        }
    }
}

/// Terminal color theme preset (`Auto` detects `NO_COLOR`, `COLORTERM`, and `TERM`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreset {
    #[default]
    Auto,
    Dark,
    Light,
    Ansi256,
    Ansi16,
    NoColor,
}

impl ThemePreset {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::Ansi256,
            Self::Ansi256 => Self::Ansi16,
            Self::Ansi16 => Self::NoColor,
            Self::NoColor => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Dark => "Midnight Dark",
            Self::Light => "Daylight Light",
            Self::Ansi256 => "ANSI 256",
            Self::Ansi16 => "ANSI 16",
            Self::NoColor => "No Color",
        }
    }
}

/// Terminal image rendering protocol preference (`auto` detects Kitty / iTerm2 / Sixel / Halfblock).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageProtocolPreference {
    #[default]
    Auto,
    Kitty,
    #[serde(alias = "iterm")]
    Iterm2,
    Sixel,
    #[serde(alias = "halfblocks", alias = "ansi")]
    Halfblock,
}

impl ImageProtocolPreference {
    pub fn parse_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "kitty" => Some(Self::Kitty),
            "iterm2" | "iterm" => Some(Self::Iterm2),
            "sixel" => Some(Self::Sixel),
            "halfblock" | "halfblocks" | "ansi" => Some(Self::Halfblock),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kitty => "kitty",
            Self::Iterm2 => "iterm2",
            Self::Sixel => "sixel",
            Self::Halfblock => "halfblock",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Kitty => "Kitty",
            Self::Iterm2 => "iTerm2",
            Self::Sixel => "Sixel",
            Self::Halfblock => "Halfblock",
        }
    }
}

/// Entry persona (`teams`), kept as an enum for stable configuration/caller ergonomics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Persona {
    #[default]
    Teams,
}

impl Persona {
    /// Detects entry persona (pure Microsoft Teams).
    pub fn detect() -> Self {
        Self::Teams
    }

    pub fn bin_name(self) -> &'static str {
        match self {
            Self::Teams => "teams",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// Entra ID Application (client) ID.
    pub client_id: String,
    /// Entra ID Tenant ID (`common`, `organizations`, domain, or tenant GUID).
    pub tenant_id: String,
    /// Optional OAuth scopes override. Empty uses the standard Microsoft Teams delegated scope set.
    pub scopes: Vec<String>,
    /// Base URL for Microsoft Graph v1.0 (overridable for sovereign clouds or mock testing).
    pub graph_base_url: String,
    /// Base URL for Entra OAuth2 endpoints (overridable for sovereign clouds or mock testing).
    pub auth_base_url: String,
    /// GitHub `owner/repo` used by `teams update`.
    pub github_repo: String,
    /// Enable desktop toast/bell notifications in TUI.
    pub notify: bool,
    /// Show left navigation rail in wide layout.
    pub show_nav_rail: bool,
    /// Automatically open right thread/info pane on wide terminals.
    pub auto_thread_pane: bool,
    /// Render inline image thumbnails directly inside terminal Chat views.
    pub inline_images: bool,
    /// Preferred terminal image rendering protocol (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`).
    pub image_protocol: ImageProtocolPreference,
    /// Custom download directory. Empty defaults to `Downloads/teams`.
    pub download_dir: String,
    /// Fast poll interval (ms) for active conversation in TUI.
    pub active_poll_ms: u64,
    /// Background chat preview diff interval (ms).
    pub chat_poll_ms: u64,
    /// Background channel delta & presence interval (ms).
    pub channel_poll_ms: u64,
    /// Optional external token file path (`TEAMS_TOKEN_FILE` or `config.toml`), read live on demand.
    pub token_file: String,
    /// Optional external token refresh command hook (`TEAMS_TOKEN_REFRESH_CMD` or `config.toml`), executed when token is missing or within `token_refresh_mins` of expiry.
    pub token_refresh_mins: u64,
    /// Refresh command string (`TEAMS_TOKEN_REFRESH_CMD`).
    pub token_refresh_cmd: String,
    /// CLI local cache TTL in seconds (`TEAMS_CLI_CACHE_SECS`, default `0`).
    /// Defaults to `0` so CLI invocations fetch fresh unless `TEAMS_CLI_CACHE_SECS`, `cli_cache_ttl_secs`, or `--cached` is used.
    pub cli_cache_ttl_secs: u64,
    /// Automatically start and attach to the lightweight background sync daemon (`TEAMS_AUTO_DAEMON=1|true`, default `false`).
    pub auto_daemon: bool,
    /// TUI message density (`cozy` or `compact`).
    pub density: DensityMode,
    /// TUI color theme preset (`auto`, `dark`, `light`, `ansi256`, `ansi16`, `nocolor`).
    pub theme: ThemePreset,
    /// Pinned conversation keys (`conv_key`) shown at the top of the Chats sidebar.
    pub pinned_convs: Vec<String>,
}

impl Default for AppConfig {
    /// Built-in defaults with the `TEAMS_*` environment overrides applied.
    fn default() -> Self {
        let mut cfg = Self::builtin();
        apply_env_overrides(&mut cfg);
        cfg
    }
}

/// A config field that an environment variable can override for one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvField {
    ClientId,
    TenantId,
    GraphBaseUrl,
    AuthBaseUrl,
    GithubRepo,
    TokenFile,
    TokenRefreshCmd,
    TokenRefreshMins,
    CliCacheTtlSecs,
    AutoDaemon,
    ImageProtocol,
}

/// Applies the `TEAMS_*` (and `AZURE_*`) overrides to `cfg` and returns the fields they set.
/// For each field the first variable that is set wins, and a blank value is ignored.
fn apply_env_overrides(cfg: &mut AppConfig) -> Vec<EnvField> {
    fn text(names: &[&str]) -> Option<String> {
        names
            .iter()
            .find_map(|name| std::env::var(name).ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
    let mut applied = Vec::new();
    if let Some(v) = text(&["TEAMS_CLIENT_ID", "AZURE_CLIENT_ID"]) {
        cfg.client_id = v;
        applied.push(EnvField::ClientId);
    }
    if let Some(v) = text(&["TEAMS_TENANT_ID", "AZURE_TENANT_ID"]) {
        cfg.tenant_id = v;
        applied.push(EnvField::TenantId);
    }
    if let Some(v) = text(&["TEAMS_GRAPH_BASE_URL"]) {
        cfg.graph_base_url = v;
        applied.push(EnvField::GraphBaseUrl);
    }
    if let Some(v) = text(&["TEAMS_AUTH_BASE_URL"]) {
        cfg.auth_base_url = v;
        applied.push(EnvField::AuthBaseUrl);
    }
    if let Some(v) = text(&["TEAMS_GITHUB_REPO"]) {
        cfg.github_repo = v;
        applied.push(EnvField::GithubRepo);
    }
    if let Some(v) = text(&["TEAMS_TOKEN_FILE"]) {
        cfg.token_file = v;
        applied.push(EnvField::TokenFile);
    }
    if let Some(v) = text(&["TEAMS_TOKEN_REFRESH_CMD"]) {
        cfg.token_refresh_cmd = v;
        applied.push(EnvField::TokenRefreshCmd);
    }
    if let Some(v) = text(&["TEAMS_TOKEN_REFRESH_MINS"]).and_then(|v| v.parse::<u64>().ok()) {
        cfg.token_refresh_mins = v;
        applied.push(EnvField::TokenRefreshMins);
    }
    if let Some(v) = text(&["TEAMS_CLI_CACHE_SECS"]).and_then(|v| v.parse::<u64>().ok()) {
        cfg.cli_cache_ttl_secs = v;
        applied.push(EnvField::CliCacheTtlSecs);
    }
    if let Some(v) = text(&["TEAMS_AUTO_DAEMON"]).and_then(|v| parse_env_bool(&v)) {
        cfg.auto_daemon = v;
        applied.push(EnvField::AutoDaemon);
    }
    if let Some(v) = text(&["TEAMS_IMAGE_PROTOCOL"]).and_then(|v| ImageProtocolPreference::parse_str(&v)) {
        cfg.image_protocol = v;
        applied.push(EnvField::ImageProtocol);
    }
    applied
}

impl AppConfig {
    /// Defaults without any environment override.
    fn builtin() -> Self {
        Self {
            client_id: DEFAULT_PUBLIC_CLIENT_ID.to_string(),
            tenant_id: DEFAULT_TENANT_ID.to_string(),
            scopes: Vec::new(),
            graph_base_url: DEFAULT_GRAPH_BASE_URL.to_string(),
            auth_base_url: DEFAULT_AUTH_BASE_URL.to_string(),
            github_repo: DEFAULT_GITHUB_REPO.to_string(),
            notify: true,
            show_nav_rail: true,
            auto_thread_pane: true,
            inline_images: true,
            image_protocol: ImageProtocolPreference::default(),
            download_dir: String::new(),
            active_poll_ms: 2_500,
            chat_poll_ms: 8_000,
            channel_poll_ms: 30_000,
            token_file: String::new(),
            token_refresh_cmd: String::new(),
            token_refresh_mins: 5,
            cli_cache_ttl_secs: 0,
            auto_daemon: false,
            density: DensityMode::default(),
            theme: ThemePreset::default(),
            pinned_convs: Vec::new(),
        }
    }

    pub fn load() -> Self {
        let Ok(mut cfg) = Self::load_from(&config_file_path()) else {
            return Self::default();
        };
        // Environment variables take precedence over file values when set.
        apply_env_overrides(&mut cfg);
        cfg
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;
        toml::from_str(&raw)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))
    }

    /// Writes the config file. Values that only come from a `TEAMS_*` variable of this process
    /// are not made permanent: see [`AppConfig::persistable`].
    pub fn save(&self) -> Result<()> {
        #[cfg(test)]
        if std::env::var_os("TEAMS_CONFIG_DIR").is_none() {
            return Ok(());
        }
        let path = config_file_path();
        self.persistable(&path).save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        ensure_parent(path)?;
        let raw = toml::to_string_pretty(self).context("Failed to serialize config")?;
        write_atomic(path, raw.as_bytes())
    }

    /// The config as it should be written to `path`. A field whose value is still the one an
    /// environment variable supplied keeps what the file holds (the built-in default when the
    /// file has no value for it); a field changed in this process (`login --tenant-id`, say)
    /// is written. Without overrides this is `self` unchanged.
    fn persistable(&self, path: &Path) -> Self {
        let mut from_env = Self::builtin();
        let overridden = apply_env_overrides(&mut from_env);
        if overridden.is_empty() {
            return self.clone();
        }
        let raw = fs::read_to_string(path).ok();
        let file_keys = raw.as_deref().and_then(|r| r.parse::<toml::Table>().ok());
        let on_disk = raw.as_deref().and_then(|r| toml::from_str::<Self>(r).ok());
        let builtin = Self::builtin();
        let mut out = self.clone();
        macro_rules! keep_file_value {
            ($field:ident) => {
                if out.$field == from_env.$field {
                    out.$field = match (&on_disk, &file_keys) {
                        (Some(disk), Some(keys)) if keys.contains_key(stringify!($field)) => {
                            disk.$field.clone()
                        }
                        _ => builtin.$field.clone(),
                    };
                }
            };
        }
        for field in overridden {
            match field {
                EnvField::ClientId => keep_file_value!(client_id),
                EnvField::TenantId => keep_file_value!(tenant_id),
                EnvField::GraphBaseUrl => keep_file_value!(graph_base_url),
                EnvField::AuthBaseUrl => keep_file_value!(auth_base_url),
                EnvField::GithubRepo => keep_file_value!(github_repo),
                EnvField::TokenFile => keep_file_value!(token_file),
                EnvField::TokenRefreshCmd => keep_file_value!(token_refresh_cmd),
                EnvField::TokenRefreshMins => keep_file_value!(token_refresh_mins),
                EnvField::CliCacheTtlSecs => keep_file_value!(cli_cache_ttl_secs),
                EnvField::AutoDaemon => keep_file_value!(auto_daemon),
                EnvField::ImageProtocol => keep_file_value!(image_protocol),
            }
        }
        out
    }

    pub fn effective_scopes(&self) -> Vec<String> {
        if !self.scopes.is_empty() {
            return self.scopes.clone();
        }
        vec![
            "offline_access".to_string(),
            "User.Read".to_string(),
            "User.ReadBasic.All".to_string(),
            "Chat.ReadWrite".to_string(),
            "Team.ReadBasic.All".to_string(),
            "Channel.ReadBasic.All".to_string(),
            "ChannelMessage.Read.All".to_string(),
            "ChannelMessage.Send".to_string(),
            "Files.ReadWrite.All".to_string(),
            "Presence.ReadWrite".to_string(),
        ]
    }

    pub fn resolved_download_dir(&self) -> PathBuf {
        if self.download_dir.trim().is_empty() {
            default_download_dir()
        } else {
            expand_tilde(self.download_dir.trim())
        }
    }

    pub fn resolved_token_file(&self) -> Option<PathBuf> {
        let trimmed = self.token_file.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(expand_tilde(trimmed))
        }
    }
}

pub fn config_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("TEAMS_CONFIG_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed);
        }
    }
    default_config_dir()
}

/// The config directory used when `TEAMS_CONFIG_DIR` is not set.
pub fn default_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

pub fn data_dir() -> PathBuf {
    if let Ok(override_dir) = std::env::var("TEAMS_DATA_DIR") {
        let trimmed = override_dir.trim();
        if !trimmed.is_empty() {
            return expand_tilde(trimmed);
        }
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR_NAME)
}

/// Resolves symlinks on the deepest existing ancestor of `path` and re-appends
/// any non-existent trailing path components. Falls back to `std::path::absolute`.
pub fn canonicalize_existing_ancestor(path: &Path) -> PathBuf {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut cur = abs.as_path();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canon) = fs::canonicalize(cur) {
            let mut out = canon;
            for comp in tail.into_iter().rev() {
                out.push(comp);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            _ => return abs,
        }
    }
}

pub fn config_file_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn credential_fallback_path() -> PathBuf {
    config_dir().join("credentials.json")
}

pub fn drafts_path() -> PathBuf {
    config_dir().join("drafts.json")
}

pub fn db_path(user_id: Option<&str>) -> PathBuf {
    let bucket = user_id
        .filter(|id| !id.trim().is_empty())
        .unwrap_or("default");
    let sanitized: String = bucket
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    data_dir().join(sanitized).join("cache.db")
}

pub fn default_download_dir() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("teams")
}

pub fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    Ok(())
}

static ATOMIC_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically writes `data` to `path` using a unique temp file (`create_new(true)`),
/// flushing before rename and avoiding naked deletion of the destination before the
/// replacement is ready. Files whose name contains `"credentials"` default to `0o600` on Unix.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let is_secret = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase().contains("credentials"))
        .unwrap_or(false);
    write_atomic_with_mode(path, data, is_secret)
}

/// Atomically writes `data` to `path` with `0o600` permissions at creation time on Unix.
pub fn write_atomic_0600(path: &Path, data: &[u8]) -> Result<()> {
    write_atomic_with_mode(path, data, true)
}

fn write_atomic_with_mode(path: &Path, data: &[u8], mode_0600: bool) -> Result<()> {
    let resolved_target: PathBuf = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            if mode_0600 {
                bail!(
                    "Refusing to write secret file through symlink: {}",
                    path.display()
                );
            }
            let resolved = fs::canonicalize(path).with_context(|| {
                format!("Refusing to write through broken symlink: {}", path.display())
            })?;
            if fs::metadata(&resolved).map(|m| m.is_dir()).unwrap_or(false) {
                bail!(
                    "Refusing to write file through symlink to directory: {}",
                    path.display()
                );
            }
            resolved
        }
        _ => path.to_path_buf(),
    };
    let dest = resolved_target.as_path();

    ensure_parent(dest)?;
    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let pid = std::process::id();
    let seq = ATOMIC_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{pid}.{seq}.tmp"));

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    if mode_0600 {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = mode_0600;

    let write_res = (|| -> Result<()> {
        let mut file = opts
            .open(&tmp)
            .with_context(|| format!("Failed to create temp file: {}", tmp.display()))?;
        file.write_all(data)
            .with_context(|| format!("Failed to write temp file: {}", tmp.display()))?;
        file.flush()
            .with_context(|| format!("Failed to flush temp file: {}", tmp.display()))?;
        Ok(())
    })();

    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }

    if let Err(rename_err) = fs::rename(&tmp, dest) {
        if let Err(replace_err) =
            replace_existing_with_backup(&tmp, dest, parent, name, pid, seq, rename_err)
        {
            let _ = fs::remove_file(&tmp);
            return Err(replace_err);
        }
    }
    Ok(())
}

fn replace_existing_with_backup(
    tmp: &Path,
    path: &Path,
    parent: &Path,
    name: &str,
    pid: u32,
    seq: u64,
    original_err: std::io::Error,
) -> Result<()> {
    if path.symlink_metadata().is_err() {
        return Err(original_err).with_context(|| format!("Failed to save file: {}", path.display()));
    }
    let backup = parent.join(format!(".{name}.{pid}.{seq}.bak"));
    fs::rename(path, &backup)
        .with_context(|| format!("Failed to back up existing file: {}", path.display()))?;
    match fs::rename(tmp, path) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(err) => {
            let _ = fs::rename(&backup, path);
            Err(err).with_context(|| format!("Atomic file save failed: {}", path.display()))
        }
    }
}

/// Draft key format: `"{conv_key}"` for conversation root or `"{conv_key}#thread:{root_msg_id}"` for thread replies.
pub fn load_drafts() -> HashMap<String, String> {
    load_drafts_from(&drafts_path()).unwrap_or_default()
}

pub fn load_drafts_from(path: &Path) -> Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(HashMap::new());
    }
    Ok(serde_json::from_str(&raw)?)
}

pub fn save_drafts(drafts: &HashMap<String, String>) -> Result<()> {
    #[cfg(test)]
    if std::env::var_os("TEAMS_CONFIG_DIR").is_none() {
        return Ok(());
    }
    save_drafts_to(drafts, &drafts_path())
}

pub fn save_drafts_to(drafts: &HashMap<String, String>, path: &Path) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(drafts)?;
    write_atomic(path, &bytes)
}

fn parse_env_bool(input: &str) -> Option<bool> {
    match input.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

pub(crate) fn expand_tilde(input: &str) -> PathBuf {
    if input == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("~"));
    }
    if let Some(rest) = input.strip_prefix("~/").or_else(|| input.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvVarGuard {
        fn set(vars: &[(&'static str, Option<&str>)]) -> Self {
            let mut saved = Vec::with_capacity(vars.len());
            for &(key, val) in vars {
                saved.push((key, std::env::var(key).ok()));
                match val {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (key, orig) in self.saved.drain(..) {
                match orig {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    /// Every variable `apply_env_overrides` reads, so a test starts from a known environment.
    const OVERRIDE_VARS: [&str; 13] = [
        "TEAMS_CLIENT_ID",
        "AZURE_CLIENT_ID",
        "TEAMS_TENANT_ID",
        "AZURE_TENANT_ID",
        "TEAMS_GRAPH_BASE_URL",
        "TEAMS_AUTH_BASE_URL",
        "TEAMS_GITHUB_REPO",
        "TEAMS_TOKEN_FILE",
        "TEAMS_TOKEN_REFRESH_CMD",
        "TEAMS_TOKEN_REFRESH_MINS",
        "TEAMS_CLI_CACHE_SECS",
        "TEAMS_AUTO_DAEMON",
        "TEAMS_IMAGE_PROTOCOL",
    ];

    fn env_for(config_dir: &Path, set: &[(&'static str, &'static str)]) -> EnvVarGuard {
        let dir: &'static str = Box::leak(config_dir.to_string_lossy().into_owned().into_boxed_str());
        let mut vars: Vec<(&'static str, Option<&str>)> = OVERRIDE_VARS.iter().map(|k| (*k, None)).collect();
        vars.push(("TEAMS_CONFIG_DIR", Some(dir)));
        for &(k, v) in set {
            vars.retain(|(name, _)| *name != k);
            vars.push((k, Some(v)));
        }
        EnvVarGuard::set(&vars)
    }

    // TEAMS-15: a one-off TEAMS_* override is not written to config.toml.
    #[test]
    fn regression_teams_15_env_override_is_not_persisted() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut on_disk = AppConfig::builtin();
        on_disk.graph_base_url = "https://graph.file.example/v1.0".to_string();
        on_disk.token_refresh_mins = 9;
        on_disk.save_to(&path).unwrap();

        {
            let _env = env_for(
                dir.path(),
                &[
                    ("TEAMS_GRAPH_BASE_URL", "http://127.0.0.1:9/v1.0"),
                    ("TEAMS_TOKEN_REFRESH_MINS", "1"),
                    ("TEAMS_AUTO_DAEMON", "1"),
                ],
            );
            let mut cfg = AppConfig::load();
            assert_eq!(cfg.graph_base_url, "http://127.0.0.1:9/v1.0");
            assert_eq!(cfg.token_refresh_mins, 1);
            assert!(cfg.auto_daemon);
            cfg.density = DensityMode::Compact; // e.g. a TUI toggle
            cfg.save().unwrap();
        }

        let saved = AppConfig::load_from(&path).unwrap();
        assert_eq!(saved.graph_base_url, "https://graph.file.example/v1.0");
        assert_eq!(saved.token_refresh_mins, 9);
        assert!(!saved.auto_daemon, "absent from the file: built-in default");
        assert_eq!(saved.density, DensityMode::Compact);
    }

    #[test]
    fn regression_teams_15_explicit_change_is_persisted_despite_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        AppConfig::builtin().save_to(&path).unwrap();
        {
            let _env = env_for(dir.path(), &[("TEAMS_TENANT_ID", "tenant-from-env")]);
            let mut cfg = AppConfig::load();
            assert_eq!(cfg.tenant_id, "tenant-from-env");
            cfg.tenant_id = "tenant-from-login".to_string(); // `login --tenant-id`
            cfg.save().unwrap();
        }
        assert_eq!(AppConfig::load_from(&path).unwrap().tenant_id, "tenant-from-login");
    }

    #[test]
    fn regression_teams_15_save_is_unchanged_without_env_and_defaults_without_file() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // No overrides: what is saved is exactly the struct.
        {
            let _env = env_for(dir.path(), &[]);
            let mut cfg = AppConfig::builtin();
            cfg.graph_base_url = "https://graph.custom.example/v1.0".to_string();
            cfg.auto_daemon = true;
            assert_eq!(cfg.persistable(&path), cfg);
            cfg.save().unwrap();
            let expected = toml::to_string_pretty(&cfg).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        }

        // No file yet, override set: the built-in default is written, not the override.
        fs::remove_file(&path).unwrap();
        {
            let _env = env_for(dir.path(), &[("TEAMS_GRAPH_BASE_URL", "http://127.0.0.1:9/v1.0")]);
            let cfg = AppConfig::load();
            assert_eq!(cfg.graph_base_url, "http://127.0.0.1:9/v1.0");
            cfg.save().unwrap();
        }
        assert_eq!(AppConfig::load_from(&path).unwrap().graph_base_url, DEFAULT_GRAPH_BASE_URL);
    }

    #[test]
    fn test_app_config_toml_parses_legacy_unknown_keys_and_cli_cache_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let legacy_path = dir.path().join("legacy.toml");
        fs::write(
            &legacy_path,
            "notify = false\nactive_poll_ms = 3000\nbackground_all_personas = true\ncli_cache_ttl_secs = 45\n",
        )
        .unwrap();

        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvVarGuard::set(&[("TEAMS_CLI_CACHE_SECS", None)]);

        let legacy_cfg = AppConfig::load_from(&legacy_path).unwrap();
        assert!(!legacy_cfg.notify);
        assert_eq!(legacy_cfg.active_poll_ms, 3000);
        assert_eq!(legacy_cfg.cli_cache_ttl_secs, 45);
    }

    #[test]
    fn test_app_config_env_overrides_cli_cache_ttl_and_pure_teams_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        fs::write(&cfg_path, "cli_cache_ttl_secs = 15\n").unwrap();

        let _lock = ENV_LOCK.lock().unwrap();
        let dir_str = dir.path().to_string_lossy().into_owned();

        {
            let _guard = EnvVarGuard::set(&[
                ("TEAMS_CONFIG_DIR", Some(&dir_str)),
                ("TEAMS_CLI_CACHE_SECS", Some("120")),
            ]);
            let cfg = AppConfig::load();
            assert_eq!(cfg.cli_cache_ttl_secs, 120);
        }

        {
            let _guard = EnvVarGuard::set(&[
                ("TEAMS_CONFIG_DIR", Some(&dir_str)),
                ("TEAMS_CLI_CACHE_SECS", None),
            ]);
            let def = AppConfig::default();
            assert_eq!(def.cli_cache_ttl_secs, 0);
            let scopes = def.effective_scopes();
            assert!(scopes.iter().any(|s| s == "Chat.ReadWrite"));
            assert!(scopes.iter().any(|s| s == "ChannelMessage.Read.All"));
            assert!(!scopes.iter().any(|s| s.starts_with("Mail.")));
            assert!(!scopes.iter().any(|s| s.starts_with("Calendars.")));
            assert!(!scopes.iter().any(|s| s.starts_with("Contacts.")));
            assert!(!scopes.iter().any(|s| s.starts_with("Tasks.")));
            assert!(!scopes.iter().any(|s| s.starts_with("Notes.")));
        }
        assert_eq!(Persona::detect(), Persona::Teams);
        assert_eq!(Persona::Teams.bin_name(), "teams");
    }

    #[test]
    fn test_parse_env_bool_values() {
        assert_eq!(parse_env_bool("1"), Some(true));
        assert_eq!(parse_env_bool(" true "), Some(true));
        assert_eq!(parse_env_bool("TRUE"), Some(true));
        assert_eq!(parse_env_bool("0"), Some(false));
        assert_eq!(parse_env_bool("false"), Some(false));
        assert_eq!(parse_env_bool("invalid"), None);
    }

    #[test]
    fn test_write_atomic_and_0600_and_missing_config_load() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sub/settings.toml");
        write_atomic(&target, b"first = 1\n").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first = 1\n");

        write_atomic(&target, b"second = 2\n").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second = 2\n");

        let secret = dir.path().join("sub/credentials.json");
        write_atomic_0600(&secret, b"{\"token\":\"abc\"}").unwrap();
        assert_eq!(fs::read(&secret).unwrap(), b"{\"token\":\"abc\"}");

        let leftovers: Vec<_> = fs::read_dir(dir.path().join("sub"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name();
                let s = n.to_string_lossy();
                s.ends_with(".tmp") || s.ends_with(".bak")
            })
            .collect();
        assert!(leftovers.is_empty());

        let _lock = ENV_LOCK.lock().unwrap();
        let missing_dir = dir.path().join("no-config-here");
        let missing_str = missing_dir.to_string_lossy().into_owned();
        let _guard = EnvVarGuard::set(&[
            ("TEAMS_CONFIG_DIR", Some(&missing_str)),
            ("TEAMS_CLI_CACHE_SECS", Some("77")),
        ]);
        let cfg = AppConfig::load();
        assert_eq!(cfg.cli_cache_ttl_secs, 77);
    }

    #[test]
    fn test_image_protocol_preference_parse_aliases_and_toml_roundtrip() {
        assert_eq!(
            ImageProtocolPreference::parse_str("auto"),
            Some(ImageProtocolPreference::Auto)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str(""),
            Some(ImageProtocolPreference::Auto)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("KITTY"),
            Some(ImageProtocolPreference::Kitty)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("iterm2"),
            Some(ImageProtocolPreference::Iterm2)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("iterm"),
            Some(ImageProtocolPreference::Iterm2)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("sixel"),
            Some(ImageProtocolPreference::Sixel)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("halfblock"),
            Some(ImageProtocolPreference::Halfblock)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("halfblocks"),
            Some(ImageProtocolPreference::Halfblock)
        );
        assert_eq!(
            ImageProtocolPreference::parse_str("ansi"),
            Some(ImageProtocolPreference::Halfblock)
        );
        assert_eq!(ImageProtocolPreference::parse_str("unknown"), None);

        assert_eq!(ImageProtocolPreference::Auto.as_str(), "auto");
        assert_eq!(ImageProtocolPreference::Kitty.as_str(), "kitty");
        assert_eq!(ImageProtocolPreference::Iterm2.as_str(), "iterm2");
        assert_eq!(ImageProtocolPreference::Sixel.as_str(), "sixel");
        assert_eq!(ImageProtocolPreference::Halfblock.as_str(), "halfblock");
        assert_eq!(ImageProtocolPreference::Auto.label(), "Auto");
        assert_eq!(ImageProtocolPreference::Kitty.label(), "Kitty");
        assert_eq!(ImageProtocolPreference::Iterm2.label(), "iTerm2");
        assert_eq!(ImageProtocolPreference::Sixel.label(), "Sixel");
        assert_eq!(ImageProtocolPreference::Halfblock.label(), "Halfblock");

        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");

        let _lock = ENV_LOCK.lock().unwrap();
        {
            let _guard = EnvVarGuard::set(&[("TEAMS_IMAGE_PROTOCOL", None)]);
            let mut cfg = AppConfig::default();
            assert_eq!(cfg.image_protocol, ImageProtocolPreference::Auto);
            cfg.image_protocol = ImageProtocolPreference::Sixel;
            cfg.save_to(&cfg_path).unwrap();

            let reloaded = AppConfig::load_from(&cfg_path).unwrap();
            assert_eq!(reloaded.image_protocol, ImageProtocolPreference::Sixel);
        }

        // Verify serde aliases (`iterm`, `halfblocks`, `ansi`) deserialize cleanly
        for (raw, expected) in [
            ("image_protocol = \"iterm\"\n", ImageProtocolPreference::Iterm2),
            (
                "image_protocol = \"halfblocks\"\n",
                ImageProtocolPreference::Halfblock,
            ),
            (
                "image_protocol = \"ansi\"\n",
                ImageProtocolPreference::Halfblock,
            ),
            (
                "image_protocol = \"kitty\"\n",
                ImageProtocolPreference::Kitty,
            ),
        ] {
            let parsed: AppConfig = toml::from_str(raw).unwrap();
            assert_eq!(parsed.image_protocol, expected);
        }

        // Verify TEAMS_IMAGE_PROTOCOL env override in AppConfig::load()
        let dir_str = dir.path().to_string_lossy().into_owned();
        {
            let _guard = EnvVarGuard::set(&[
                ("TEAMS_CONFIG_DIR", Some(&dir_str)),
                ("TEAMS_IMAGE_PROTOCOL", Some("iterm")),
            ]);
            let cfg = AppConfig::load();
            assert_eq!(cfg.image_protocol, ImageProtocolPreference::Iterm2);
        }
    }

    fn try_create_file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).unwrap();
            true
        }
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_file(target, link) {
                Ok(()) => true,
                Err(err)
                    if err.kind() == std::io::ErrorKind::PermissionDenied
                        || err.raw_os_error() == Some(1314) =>
                {
                    false
                }
                Err(err) => panic!("unexpected symlink_file error: {err}"),
            }
        }
    }

    fn try_create_dir_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).unwrap();
            true
        }
        #[cfg(windows)]
        {
            match std::os::windows::fs::symlink_dir(target, link) {
                Ok(()) => true,
                Err(err)
                    if err.kind() == std::io::ErrorKind::PermissionDenied
                        || err.raw_os_error() == Some(1314) =>
                {
                    false
                }
                Err(err) => panic!("unexpected symlink_dir error: {err}"),
            }
        }
    }

    #[test]
    fn test_canonicalize_existing_ancestor_resolves_tail_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("real");
        fs::create_dir_all(&existing).unwrap();

        let canon_existing = fs::canonicalize(&existing).unwrap();
        assert_eq!(canonicalize_existing_ancestor(&existing), canon_existing);

        let missing_tail = existing.join("nested").join("profile");
        assert_eq!(
            canonicalize_existing_ancestor(&missing_tail),
            canon_existing.join("nested").join("profile")
        );

        let link = dir.path().join("link-home");
        if try_create_dir_symlink(&existing, &link) {
            assert_eq!(canonicalize_existing_ancestor(&link), canon_existing);
            assert_eq!(
                canonicalize_existing_ancestor(&link.join("nested").join("profile")),
                canon_existing.join("nested").join("profile")
            );
        }
    }

    #[test]
    fn test_config_and_data_dir_expand_tilde() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let _guard = EnvVarGuard::set(&[
            ("TEAMS_CONFIG_DIR", Some("~/.config/teams-custom")),
            ("TEAMS_DATA_DIR", Some("~/teams-data-custom")),
        ]);
        assert_eq!(config_dir(), home.join(".config/teams-custom"));
        assert_eq!(data_dir(), home.join("teams-data-custom"));
    }

    #[test]
    fn test_write_atomic_symlink_handling_and_0600_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles_dir = dir.path().join("dotfiles");
        let cfg_dir = dir.path().join("cfg");
        fs::create_dir_all(&dotfiles_dir).unwrap();
        fs::create_dir_all(&cfg_dir).unwrap();

        let real_cfg = dotfiles_dir.join("config.toml");
        fs::write(&real_cfg, b"notify = true\n").unwrap();
        let cfg_link = cfg_dir.join("config.toml");
        if !try_create_file_symlink(&real_cfg, &cfg_link) {
            return;
        }

        // Non-secret write through symlink writes to real target and preserves symlink.
        write_atomic(&cfg_link, b"notify = false\n").unwrap();
        assert!(fs::symlink_metadata(&cfg_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real_cfg).unwrap(), b"notify = false\n");
        assert_eq!(fs::read(&cfg_link).unwrap(), b"notify = false\n");

        // Dangling symlink must fail.
        let dangling_link = cfg_dir.join("dangling.toml");
        assert!(try_create_file_symlink(
            &dotfiles_dir.join("missing.toml"),
            &dangling_link
        ));
        let err = write_atomic(&dangling_link, b"x = 1\n").unwrap_err();
        assert!(err.to_string().contains("broken symlink"), "{err}");

        // Symlink to directory must fail.
        let sub_dir = dotfiles_dir.join("subdir");
        fs::create_dir_all(&sub_dir).unwrap();
        let dir_link = cfg_dir.join("dir_link.toml");
        if try_create_dir_symlink(&sub_dir, &dir_link) {
            let err = write_atomic(&dir_link, b"x = 1\n").unwrap_err();
            assert!(err.to_string().contains("directory"), "{err}");
        }

        // Secret writes (0600 or credentials.json) must refuse symlinks without touching target.
        let real_secret = dotfiles_dir.join("secret.json");
        fs::write(&real_secret, b"original-secret").unwrap();
        let secret_link = cfg_dir.join("credentials.json");
        assert!(try_create_file_symlink(&real_secret, &secret_link));

        let err_auto = write_atomic(&secret_link, b"overwrite").unwrap_err();
        assert!(
            err_auto
                .to_string()
                .contains("Refusing to write secret file through symlink"),
            "{err_auto}"
        );
        let err_0600 = write_atomic_0600(&secret_link, b"overwrite").unwrap_err();
        assert!(
            err_0600
                .to_string()
                .contains("Refusing to write secret file through symlink"),
            "{err_0600}"
        );
        assert_eq!(fs::read(&real_secret).unwrap(), b"original-secret");
    }
}
