//! Runtime bootstrap, fast SQLite path resolution, cache cursors, and daemon control.

use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Result};
use base64::Engine as _;
use serde_json::json;

use crate::{
    auth::AuthManager,
    config::{self, AppConfig, Persona},
    graph::GraphClient,
    models::SyncCursor,
    storage::Storage,
};

use super::args::DaemonCommand;

pub(super) fn print_daemon_status_text(header: &str, cfg: &AppConfig, status: &crate::daemon::DaemonStatus) {
    println!(
        "{header}: running={}, pid={}, transport={}, endpoint={}, attached_clients={}, auto_daemon={}",
        status.running,
        status
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string()),
        status.transport.as_deref().unwrap_or("-"),
        status.endpoint.as_deref().unwrap_or("-"),
        status.attached_clients,
        cfg.auto_daemon,
    );
    if status.running {
        println!(
            "  version={}, uptime_secs={}, sync_busy={}, last_sync_at={}",
            status.version.as_deref().unwrap_or("-"),
            status
                .uptime_secs
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string()),
            status.sync_busy,
            status.last_sync_at.as_deref().unwrap_or("-"),
        );
        if !status.sync_summary.trim().is_empty() {
            println!("  sync_summary: {}", status.sync_summary);
        }
    }
    if let Some(ref err) = status.last_error {
        println!("  last_error: {err}");
    }
}

pub(super) async fn dispatch_daemon_command(cfg: &AppConfig, command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Start { json } => {
            let status =
                crate::daemon::ensure_daemon_started(cfg, current_user_id_fast(cfg).as_deref())
                    .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon started", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Stop { json } => {
            let stopped = crate::daemon::stop_daemon(cfg).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "ok": true,
                        "stopped": stopped,
                    }))?
                );
            } else if stopped {
                println!("Background sync daemon stopped.");
            } else {
                println!("Background sync daemon is not running.");
            }
            Ok(())
        }
        DaemonCommand::Status { json } => {
            let status = crate::daemon::query_daemon_status(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon status", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Restart { json } => {
            let _ = crate::daemon::stop_daemon(cfg).await?;
            let status =
                crate::daemon::ensure_daemon_started(cfg, current_user_id_fast(cfg).as_deref())
                    .await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_daemon_status_text("daemon restarted", cfg, &status);
            }
            Ok(())
        }
        DaemonCommand::Run => crate::daemon::run_daemon_foreground(cfg.clone()).await,
    }
}


pub(super) async fn open_runtime(
    cfg: &AppConfig,
    require_login: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    if require_login && tokens.is_none() {
        bail!(
            "Sign-in required. Run `{} login` first.",
            Persona::detect().bin_name()
        );
    }
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());
    let storage = Storage::open(&config::db_path(user_id))?;
    let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
    if let Some(uid) = user_id {
        graph = graph.with_my_user_id(uid);
    }
    Ok((auth, graph, storage))
}

pub(super) fn db_path_in_dir(data_dir: &Path, user_id: Option<&str>) -> PathBuf {
    let default_path = config::db_path(user_id);
    if let Ok(rel) = default_path.strip_prefix(config::data_dir()) {
        return data_dir.join(rel);
    }
    let bucket = default_path
        .parent()
        .and_then(|p| p.file_name())
        .unwrap_or_else(|| std::ffi::OsStr::new("default"));
    data_dir.join(bucket).join("cache.db")
}

pub(super) fn extract_user_id_from_jwt(raw_token: &str) -> Option<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let trimmed = raw_token.trim();
    let token = if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        trimmed[7..].trim()
    } else {
        trimmed
    };
    let payload_b64 = token.split('.').nth(1)?.trim();
    if payload_b64.is_empty() {
        return None;
    }
    let normalized: String = payload_b64
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            _ => c,
        })
        .collect();
    let decoded = URL_SAFE_NO_PAD.decode(&normalized).ok()?;
    let val: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    val.get("oid")
        .or_else(|| val.get("sub"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(super) fn extract_user_id_from_token_file(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cleaned = raw.trim_start_matches('\u{feff}').trim();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.starts_with('{') {
        let val: serde_json::Value = serde_json::from_str(cleaned).ok()?;
        for key in ["user_id", "userId"] {
            if let Some(uid) = val
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(uid.to_string());
            }
        }
        for key in ["access_token", "accessToken", "token"] {
            if let Some(tok) = val.get(key).and_then(|v| v.as_str()) {
                if let Some(uid) = extract_user_id_from_jwt(tok) {
                    return Some(uid);
                }
            }
        }
        return None;
    }
    let first_line = cleaned.lines().map(str::trim).find(|l| !l.is_empty())?;
    extract_user_id_from_jwt(first_line)
}

pub(super) fn find_single_existing_db(data_dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(data_dir).ok()?;
    let mut found: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let db = path.join("cache.db");
            if db.is_file() {
                if found.is_some() {
                    return None;
                }
                found = Some(db);
            }
        } else if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("cache-") && name.ends_with(".db") {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(path);
                }
            }
        }
    }
    found
}

pub(super) fn resolve_storage_db_path_fast(
    cfg: &AppConfig,
    env_access_token: Option<&str>,
    data_dir: &Path,
    cred_path: &Path,
) -> Option<PathBuf> {
    if let Some(raw_env) = env_access_token.map(str::trim).filter(|s| !s.is_empty()) {
        let uid = extract_user_id_from_jwt(raw_env);
        return Some(db_path_in_dir(data_dir, uid.as_deref()));
    }

    if let Some(tf) = cfg.resolved_token_file() {
        if tf.is_file() {
            let uid = extract_user_id_from_token_file(&tf);
            return Some(db_path_in_dir(data_dir, uid.as_deref()));
        }
    }

    if cred_path.is_file() {
        if let Some(uid) = extract_user_id_from_token_file(cred_path) {
            return Some(db_path_in_dir(data_dir, Some(&uid)));
        }
    }

    if let Some(single_db) = find_single_existing_db(data_dir) {
        return Some(single_db);
    }

    None
}

pub(super) fn infer_user_id_fast_with(
    cfg: &AppConfig,
    env_access_token: Option<&str>,
    cred_path: &Path,
) -> Option<String> {
    if let Some(raw_env) = env_access_token.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(uid) = extract_user_id_from_jwt(raw_env) {
            return Some(uid);
        }
    }
    if let Some(tf) = cfg.resolved_token_file() {
        if tf.is_file() {
            if let Some(uid) = extract_user_id_from_token_file(&tf) {
                return Some(uid);
            }
        }
    }
    if cred_path.is_file() {
        if let Some(uid) = extract_user_id_from_token_file(cred_path) {
            return Some(uid);
        }
    }
    None
}

pub(super) async fn resolve_daemon_warm(cfg: &AppConfig, refresh: bool, cached: bool) -> bool {
    if refresh || cached {
        return false;
    }
    let env_tok = std::env::var("TEAMS_ACCESS_TOKEN").ok();
    let cred_path = config::credential_fallback_path();
    let inferred_user_id = infer_user_id_fast_with(cfg, env_tok.as_deref(), &cred_path);
    let daemon_warm = crate::daemon::is_daemon_alive_fast(cfg, inferred_user_id.as_deref());
    if cfg.auto_daemon && !daemon_warm {
        let _ = crate::daemon::ensure_daemon_started(cfg, inferred_user_id.as_deref()).await;
    }
    daemon_warm
}

/// The signed-in account as far as it can be told without the OS keyring or the network.
pub(super) fn current_user_id_fast(cfg: &AppConfig) -> Option<String> {
    let env_tok = std::env::var("TEAMS_ACCESS_TOKEN").ok();
    infer_user_id_fast_with(cfg, env_tok.as_deref(), &config::credential_fallback_path())
}

/// Opens local SQLite `Storage` without touching OS Keyring or initializing `reqwest::Client` TLS roots
/// whenever `TEAMS_ACCESS_TOKEN`, `TEAMS_TOKEN_FILE`, `credentials.json`, or a single local `cache.db` resolves `user_id`.
pub(crate) fn open_storage_only(cfg: &AppConfig) -> Result<Storage> {
    let env_tok = std::env::var("TEAMS_ACCESS_TOKEN").ok();
    if let Some(path) = resolve_storage_db_path_fast(
        cfg,
        env_tok.as_deref(),
        &config::data_dir(),
        &config::credential_fallback_path(),
    ) {
        return Storage::open(&path);
    }

    // Fallback to OS keyring only when disk sources and single-DB lookup are ambiguous.
    let uid = crate::auth::load_stored_tokens()
        .ok()
        .flatten()
        .and_then(|t| t.user_id);
    Storage::open(&config::db_path(uid.as_deref()))
}

#[cfg(test)]
pub(super) fn parse_cli_cache_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0)
}

pub(super) fn cli_cache_ttl_secs(cfg: &AppConfig) -> u64 {
    if let Ok(raw) = std::env::var("TEAMS_CLI_CACHE_SECS") {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            return parsed;
        }
    }
    cfg.cli_cache_ttl_secs
}

pub(super) fn is_cursor_fresh(storage: &Storage, scope_keys: &[&str], ttl_secs: u64) -> bool {
    if ttl_secs == 0 {
        return false;
    }
    let now = chrono::Utc::now();
    for key in scope_keys {
        if let Ok(Some(cursor)) = storage.get_sync_cursor(key) {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&cursor.synced_at) {
                let age = now.signed_duration_since(dt.with_timezone(&chrono::Utc));
                if age.num_seconds() >= 0 && (age.num_seconds() as u64) <= ttl_secs {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn record_cli_sync_cursor(storage: &Storage, scope_key: &str) {
    let cursor = crate::models::SyncCursor {
        scope_key: scope_key.to_string(),
        delta_link: None,
        oldest_msg_ts: None,
        synced_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = storage.save_sync_cursor(&cursor);
}

pub(crate) fn record_chats_list_cursor(
    storage: &Storage,
    next_link: Option<String>,
    eof: bool,
) {
    let cursor = SyncCursor {
        scope_key: SyncCursor::CHATS_LIST_SCOPE.to_string(),
        delta_link: if eof { None } else { next_link },
        oldest_msg_ts: Some(if eof {
            SyncCursor::HISTORY_EOF.to_string()
        } else {
            String::new()
        }),
        synced_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = storage.save_sync_cursor(&cursor);
}

pub(crate) fn chats_cache_satisfies_limit(
    storage: &Storage,
    cached_len: usize,
    limit: usize,
) -> bool {
    cached_len >= limit
        || storage
            .get_sync_cursor(SyncCursor::CHATS_LIST_SCOPE)
            .ok()
            .flatten()
            .is_some_and(|c| c.is_history_eof())
}

pub(super) fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim_end_matches(&['\r', '\n'][..]).to_string())
}

