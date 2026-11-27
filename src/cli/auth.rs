//! Authentication, diagnostics (`whoami`, `doctor`), TUI launch, and self-update commands.

use std::path::Path;

use anyhow::{bail, Result};
use serde_json::json;

use crate::{
    auth::{diagnose_access_token_scopes, AuthManager, LoginOptions, StoredTokens},
    config::{self, AppConfig, Persona},
    graph::GraphClient,
    img_preview,
    safe_text::sanitize_terminal_inline,
    storage::Storage,
    tui, updater,
};

use super::{
    format::{format_unix_ts, wrap_graph_permission_error, DetailedError},
    runtime::open_runtime,
};

pub(super) async fn cmd_tui(cfg: &mut AppConfig, persona: Persona) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    if auth.current_tokens().await.is_none() {
        println!("No stored sign-in credentials found. Starting Microsoft account sign-in.");
        println!(
            "(To use a custom Azure App: {} login --client-id <APP_ID> --tenant-id <TENANT_ID>)",
            persona.bin_name()
        );
        cmd_login(cfg, LoginOptions::default(), false).await?;
        println!();
    }

    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    tui::run(cfg.clone(), auth, graph, storage).await
}

pub(super) async fn cmd_login(cfg: &mut AppConfig, opts: LoginOptions, json_out: bool) -> Result<()> {
    let mut config_changed = false;
    if let Some(ref cid) = opts.client_id {
        let trimmed = cid.trim();
        if !trimmed.is_empty() && cfg.client_id != trimmed {
            cfg.client_id = trimmed.to_string();
            config_changed = true;
        }
    }
    if let Some(ref tid) = opts.tenant_id {
        let trimmed = tid.trim();
        if !trimmed.is_empty() && cfg.tenant_id != trimmed {
            cfg.tenant_id = trimmed.to_string();
            config_changed = true;
        }
    }
    if config_changed {
        cfg.save()?;
    }

    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.login(&opts).await?;
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let me = graph.get_me().await?;
    let email = me
        .mail
        .clone()
        .or_else(|| me.user_principal_name.clone());
    auth.update_identity(me.id.clone(), me.display_name.clone(), email.clone())
        .await?;

    let updated = auth.current_tokens().await.unwrap_or(tokens);
    // A daemon still signed in as the previous account would keep syncing it.
    if let Err(err) = crate::daemon::stop_daemon_if_other_user(cfg, &me.id).await {
        eprintln!("warning: could not stop the background daemon of the previous account: {err:#}");
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "user": me,
                "client_id": updated.client_id,
                "tenant_id": updated.tenant_id,
                "auth_mode": updated.auth_mode,
                "expires_at": updated.expires_at,
            }))?
        );
    } else {
        println!(
            "Signed in as: {} ({}) [id={}]",
            me.display_name,
            email.as_deref().unwrap_or("-"),
            me.id
        );
        println!(
            "Auth mode: {} · tenant={} · expires: {}",
            updated.auth_mode,
            updated.tenant_id,
            format_unix_ts(updated.expires_at)
        );
    }
    Ok(())
}

pub(super) async fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    // The daemon holds its own copy of the tokens: stop it so it does not sync on as the
    // account that just signed out. Logout itself never fails because of it.
    if let Err(err) = crate::daemon::stop_daemon(cfg).await {
        eprintln!("warning: could not stop the background daemon: {err:#}");
    }
    let auth = AuthManager::new(cfg.clone())?;
    auth.logout().await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("Signed out and removed stored tokens.");
    }
    Ok(())
}

pub(super) async fn cmd_whoami(cfg: &AppConfig, persona: Persona, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let mut tokens = match auth.current_tokens().await {
        Some(t) => t,
        None => {
            if let Some(ref tf) = cfg.resolved_token_file() {
                if !tf.exists() {
                    bail!(
                        "Sign-in required. Run `{} login` or set `TEAMS_TOKEN_FILE` / `TEAMS_ACCESS_TOKEN`. (Configured TEAMS_TOKEN_FILE not found: {})",
                        persona.bin_name(),
                        tf.display()
                    );
                }
                bail!(
                    "Sign-in required. Run `{} login` or set a valid `TEAMS_TOKEN_FILE` ({}) / `TEAMS_ACCESS_TOKEN`.",
                    persona.bin_name(),
                    tf.display()
                );
            }
            bail!(
                "Sign-in required. Run `{} login` or set `TEAMS_TOKEN_FILE` / `TEAMS_ACCESS_TOKEN`.",
                persona.bin_name()
            );
        }
    };

    let refresh_skew_secs = (cfg.token_refresh_mins.max(1) * 60) as i64;
    let mut refresh_failure: Option<anyhow::Error> = None;
    if tokens.is_expired_soon(refresh_skew_secs) && auth.is_refreshable().await {
        match auth.refresh_now().await {
            Ok(refreshed) => {
                tokens = refreshed;
            }
            Err(refresh_err) => {
                if tokens.is_expired_soon(0) {
                    refresh_failure = Some(refresh_err);
                } else {
                    eprintln!(
                        "Warning: automatic token refresh failed ({refresh_err}); continuing with existing token (expires: {}).",
                        format_unix_ts(tokens.expires_at)
                    );
                }
            }
        }
    }

    let scope_diag = diagnose_access_token_scopes(&tokens.access_token);

    if tokens.is_expired_soon(0) {
        let err_detail = match refresh_failure {
            Some(ref err) => format!(
                "Token expired (exp: {}, auth_mode={}) — automatic refresh failed: {err:#}",
                format_unix_ts(tokens.expires_at),
                tokens.auth_mode
            ),
            None => format!(
                "Token expired (exp: {}, auth_mode={}) — refresh the token and retry.",
                format_unix_ts(tokens.expires_at),
                tokens.auth_mode
            ),
        };
        if json_out {
            let details = json!({
                "persona": persona,
                "user_id": tokens.user_id,
                "display_name": tokens.user_display_name,
                "email": tokens.user_email,
                "client_id": tokens.client_id,
                "tenant_id": tokens.tenant_id,
                "auth_mode": tokens.auth_mode,
                "expires_at": tokens.expires_at,
                "expires_at_human": format_unix_ts(tokens.expires_at),
                "expired": true,
                "expiring_soon": true,
                "status": "expired",
                "scopes": scope_diag.scopes,
                "roles": scope_diag.roles,
                "scope_warnings": scope_diag.warnings,
                "error": format!("Token expired (exp: {}) — refresh and retry", format_unix_ts(tokens.expires_at)),
            });
            return Err(DetailedError {
                message: format!(
                    "Token expired (exp: {}) — refresh the token and retry.",
                    format_unix_ts(tokens.expires_at)
                ),
                details,
            }
            .into());
        }
        bail!("{err_detail}");
    }

    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let mut profile_err: Option<anyhow::Error> = None;
    let live_profile = if tokens.user_id.is_none() || tokens.user_display_name.is_none() {
        match graph.get_me().await {
            Ok(p) => Some(p),
            Err(e) => {
                profile_err = Some(e);
                None
            }
        }
    } else {
        None
    };

    let user_id = tokens
        .user_id
        .clone()
        .or_else(|| live_profile.as_ref().map(|p| p.id.clone()));
    let display_name = tokens
        .user_display_name
        .clone()
        .or_else(|| live_profile.as_ref().map(|p| p.display_name.clone()));
    let email = tokens.user_email.clone().or_else(|| {
        live_profile
            .as_ref()
            .and_then(|p| p.mail.clone().or_else(|| p.user_principal_name.clone()))
    });

    if display_name.is_none() && user_id.is_none() {
        if let Some(err) = profile_err {
            bail!(
                "Failed to fetch profile (/me) (auth_mode={}, expires: {}): {err:#}",
                tokens.auth_mode,
                format_unix_ts(tokens.expires_at)
            );
        }
    }

    let expiring_soon =
        !tokens.is_expired_soon(0) && tokens.is_expired_soon(refresh_skew_secs);

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "persona": persona,
                "user_id": user_id,
                "display_name": display_name,
                "email": email,
                "client_id": tokens.client_id,
                "tenant_id": tokens.tenant_id,
                "auth_mode": tokens.auth_mode,
                "expires_at": tokens.expires_at,
                "expires_at_human": format_unix_ts(tokens.expires_at),
                "expired": false,
                "expiring_soon": expiring_soon,
                "status": if expiring_soon { "expiring_soon" } else { "valid" },
                "scopes": scope_diag.scopes,
                "roles": scope_diag.roles,
                "scope_warnings": scope_diag.warnings,
            }))?
        );
    } else {
        println!("persona: {}", persona.bin_name());
        println!(
            "user: {} ({})",
            sanitize_terminal_inline(display_name.as_deref().unwrap_or("-")),
            sanitize_terminal_inline(email.as_deref().unwrap_or("-"))
        );
        println!("user_id: {}", user_id.as_deref().unwrap_or("-"));
        println!("client_id: {}", tokens.client_id);
        println!("tenant_id: {}", tokens.tenant_id);
        println!("auth_mode: {}", tokens.auth_mode);
        println!(
            "token_expires: {} ({})",
            format_unix_ts(tokens.expires_at),
            if expiring_soon {
                "expiring soon — refresh recommended"
            } else {
                "valid"
            }
        );
        if !scope_diag.scopes.is_empty() {
            println!("token_scopes (scp): {}", scope_diag.scopes.join(" "));
        } else if !scope_diag.has_scope_claims {
            println!("token_scopes (scp): (no JWT claims)");
        }
        if !scope_diag.roles.is_empty() {
            println!("token_roles: {}", scope_diag.roles.join(" "));
        }
        for warning in &scope_diag.warnings {
            println!("scope_warning: {warning}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_doctor(
    cfg: &AppConfig,
    persona: Persona,
    probe: bool,
    json_out: bool,
) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth.current_tokens().await;
    let user_id = tokens.as_ref().and_then(|t| t.user_id.as_deref());
    let token_diag = tokens
        .as_ref()
        .map(|t| diagnose_access_token_scopes(&t.access_token))
        .unwrap_or_default();

    let cfg_path = config::config_file_path();
    let cred_path = config::credential_fallback_path();
    let drafts = config::drafts_path();
    let db = config::db_path(user_id);
    let dl_dir = cfg.resolved_download_dir();
    let configured_scopes = cfg.effective_scopes();
    let token_file = cfg.resolved_token_file();
    let token_refresh_cmd_configured = !cfg.token_refresh_cmd.trim().is_empty();
    let auth_mode = tokens.as_ref().map(|t| t.auth_mode.clone());
    let token_expires_at = tokens.as_ref().map(|t| format_unix_ts(t.expires_at));

    let keyring_status = describe_keyring_status(tokens.as_ref(), &cred_path);

    let (cached_conversations, cached_teams) = if db.exists() {
        if let Ok(storage) = Storage::open(&db) {
            (
                storage.list_conversations().map(|v| v.len()).unwrap_or(0),
                storage.list_teams().map(|v| v.len()).unwrap_or(0),
            )
        } else {
            (0, 0)
        }
    } else {
        (0, 0)
    };

    let daemon_status = crate::daemon::query_daemon_status(cfg)
        .await
        .unwrap_or_default();
    let graphics_caps = img_preview::detect_graphics_caps(cfg.image_protocol);
    // Extra trust roots on top of the bundled and OS stores (corporate TLS-inspecting proxies).
    let extra_ca: Vec<serde_json::Value> = crate::http::ca_sources()
        .into_iter()
        .map(|s| match s.outcome {
            Ok(certs) => json!({ "var": s.var, "path": s.path, "certificates": certs.len() }),
            Err(err) => json!({ "var": s.var, "path": s.path, "error": err }),
        })
        .collect();

    let mut probe_me: Option<serde_json::Value> = None;
    let mut probe_presence: Option<serde_json::Value> = None;
    let mut probe_error: Option<String> = None;

    if probe {
        let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
        if let Some(uid) = user_id {
            graph = graph.with_my_user_id(uid);
        }
        let (me_res, presence_res) = tokio::join!(graph.get_me(), graph.get_my_presence());
        match me_res {
            Ok(me) => {
                probe_me = Some(serde_json::to_value(&me)?);
                match presence_res {
                    Ok(p) => probe_presence = Some(serde_json::to_value(&p)?),
                    Err(err) => {
                        let wrapped = wrap_graph_permission_error(&graph, err).await;
                        probe_error = Some(format!("presence probe failed: {wrapped:#}"));
                    }
                }
            }
            Err(err) => {
                let wrapped = wrap_graph_permission_error(&graph, err).await;
                probe_error = Some(format!("get_me probe failed: {wrapped:#}"));
            }
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "persona": persona,
                "scopes": token_diag.scopes,
                "roles": token_diag.roles,
                "scope_warnings": token_diag.warnings,
                "configured_scopes": configured_scopes,
                "config_path": cfg_path,
                "config_exists": cfg_path.exists(),
                "credentials_fallback_path": cred_path,
                "credentials_fallback_exists": cred_path.exists(),
                "drafts_path": drafts,
                "db_path": db,
                "db_exists": db.exists(),
                "download_dir": dl_dir,
                "github_repo": cfg.github_repo,
                "client_id": cfg.client_id,
                "tenant_id": cfg.tenant_id,
                "graph_base_url": cfg.graph_base_url,
                "auth_base_url": cfg.auth_base_url,
                "keyring_status": keyring_status,
                "token_file": token_file,
                "token_file_exists": token_file.as_ref().map(|p| p.exists()),
                "token_refresh_cmd_configured": token_refresh_cmd_configured,
                "token_refresh_mins": cfg.token_refresh_mins,
                "auth_mode": auth_mode,
                "token_expires_at": token_expires_at,
                "logged_in": tokens.is_some(),
                "cache_counts": {
                    "conversations": cached_conversations,
                    "teams": cached_teams,
                },
                "daemon": {
                    "running": daemon_status.running,
                    "pid": daemon_status.pid,
                    "transport": daemon_status.transport,
                    "endpoint": daemon_status.endpoint,
                    "attached_clients": daemon_status.attached_clients,
                    "auto_daemon": cfg.auto_daemon,
                },
                "image_protocol": graphics_caps.protocol.as_str(),
                "image_protocol_preference": cfg.image_protocol.as_str(),
                "cell_px": [graphics_caps.cell_px.0, graphics_caps.cell_px.1],
                "tls": {
                    "roots": ["bundled", "os"],
                    "extra_ca": extra_ca,
                },
                "probe": if probe {
                    Some(json!({
                        "ok": probe_error.is_none(),
                        "me": probe_me,
                        "presence": probe_presence,
                        "error": probe_error,
                    }))
                } else {
                    None
                },
            }))?
        );
    } else {
        println!("{} v{} (persona={})", persona.bin_name(), env!("CARGO_PKG_VERSION"), persona.bin_name());
        println!("config.toml: {} (exists={})", cfg_path.display(), cfg_path.exists());
        println!(
            "credentials.json: {} (exists={})",
            cred_path.display(),
            cred_path.exists()
        );
        println!("drafts.json: {}", drafts.display());
        println!("cache.db: {} (exists={})", db.display(), db.exists());
        println!(
            "cache_counts: conversations={cached_conversations}, teams={cached_teams}"
        );
        println!("download_dir: {}", dl_dir.display());
        println!("github_repo: {}", cfg.github_repo);
        println!(
            "configured_scopes ({}): {}",
            configured_scopes.len(),
            configured_scopes.join(", ")
        );
        if !token_diag.scopes.is_empty() {
            println!(
                "token_scopes ({}): {}",
                token_diag.scopes.len(),
                token_diag.scopes.join(" ")
            );
        } else if tokens.is_some() {
            println!("token_scopes: (no JWT scp claims)");
        }
        if !token_diag.roles.is_empty() {
            println!(
                "token_roles ({}): {}",
                token_diag.roles.len(),
                token_diag.roles.join(" ")
            );
        }
        for warning in &token_diag.warnings {
            println!("scope_warning: {warning}");
        }
        println!("client_id: {}", cfg.client_id);
        println!("tenant_id: {}", cfg.tenant_id);
        println!("graph_base_url: {}", cfg.graph_base_url);
        println!("auth_base_url: {}", cfg.auth_base_url);
        println!("keyring_status: {keyring_status}");
        println!(
            "daemon: running={}, pid={}, endpoint={}, attached_clients={}, auto_daemon={}",
            daemon_status.running,
            daemon_status
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
            daemon_status.endpoint.as_deref().unwrap_or("-"),
            daemon_status.attached_clients,
            cfg.auto_daemon,
        );
        println!(
            "token_file: {}",
            token_file
                .as_ref()
                .map(|p| format!("{} (exists={})", p.display(), p.exists()))
                .unwrap_or_else(|| "(unset)".to_string())
        );
        println!(
            "token_refresh_cmd_configured: {token_refresh_cmd_configured} (token_refresh_mins={})",
            cfg.token_refresh_mins
        );
        println!("auth_mode: {}", auth_mode.as_deref().unwrap_or("-"));
        println!(
            "token_expires_at: {}",
            token_expires_at.as_deref().unwrap_or("-")
        );
        let cell_suffix = if graphics_caps.cell_px_exact {
            ""
        } else {
            " est."
        };
        println!(
            "Image protocol: {} (preference: {}, cell: {}x{}px{})",
            graphics_caps.protocol.label(),
            cfg.image_protocol.as_str(),
            graphics_caps.cell_px.0,
            graphics_caps.cell_px.1,
            cell_suffix,
        );
        if extra_ca.is_empty() {
            println!("tls_extra_ca: (none; bundled + OS roots)");
        }
        for source in &extra_ca {
            let var = source["var"].as_str().unwrap_or("-");
            let path = source["path"].as_str().unwrap_or("-");
            match source.get("certificates") {
                Some(n) => println!("tls_extra_ca: {var}={path} ({n} certificates)"),
                None => println!(
                    "tls_extra_ca: {var}={path} (not loaded: {})",
                    source["error"].as_str().unwrap_or("-")
                ),
            }
        }
        if probe {
            if let Some(err) = probe_error {
                println!("probe: failed ({err})");
            } else {
                println!(
                    "probe: ok (me={}, presence={})",
                    probe_me
                        .as_ref()
                        .and_then(|v| v.get("display_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("-"),
                    probe_presence
                        .as_ref()
                        .and_then(|v| v.get("availability"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("-"),
                );
            }
        }
    }

    Ok(())
}

pub(super) fn describe_keyring_status(tokens: Option<&StoredTokens>, fallback_path: &Path) -> String {
    match tokens {
        None => "not signed in (no stored token)".to_string(),
        Some(tok) if tok.auth_mode == "env_token" => {
            "env_token (using TEAMS_ACCESS_TOKEN)".to_string()
        }
        Some(tok) if tok.auth_mode == "token_file" => {
            "token_file (using TEAMS_TOKEN_FILE)".to_string()
        }
        Some(_) if fallback_path.exists() => {
            "using credentials.json file fallback".to_string()
        }
        Some(_) => "OS keyring active (token loaded)".to_string(),
    }
}

pub(super) async fn cmd_update(
    cfg: &AppConfig,
    check: bool,
    force: bool,
    repo_override: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let repo = repo_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&cfg.github_repo);
    let result = updater::check_or_apply_update(repo, check, force).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Repository: {repo}");
        println!("Current version: v{}", result.current_version);
        println!("Latest version: v{}", result.latest_version);
        if result.updated {
            println!(
                "Update complete: {} ({})",
                result.binary_path.display(),
                result.asset_name.as_deref().unwrap_or("-")
            );
        } else if result.update_available {
            println!("New version available: {}", result.release_url);
            if check {
                println!("Running in --check mode; binary was not replaced.");
            }
        } else {
            println!("Already up to date.");
        }
    }
    Ok(())
}
