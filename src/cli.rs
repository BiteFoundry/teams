//! `teams` CLI parser, command dispatcher, and shared domain helpers for CLI/HTTP/MCP.

use std::{
    collections::HashMap,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc};
use clap::{Parser, Subcommand};
use futures::stream::{self, StreamExt};
use serde_json::json;

use crate::{
    auth::{
        diagnose_access_token_scopes, explain_graph_permission_error, AuthManager, LoginOptions,
        StoredTokens,
    },
    config::{self, AppConfig, Persona},
    graph::GraphClient,
    img_preview,
    models::{
        AttachmentInfo, ConversationKind, ConversationSummary, ConversationTarget, DownloadedFile,
        InlineImageUpload, MentionInfo, MessageItem, QuotedReply, SyncCursor, TeamSummary,
        ThreadView,
    },
    parser, resolver,
    serve::{self, ServeOptions},
    storage::Storage,
    sync, tui, updater,
};

const DELETE_JSON_REQUIRES_YES: &str = "Deleting with --json requires passing --yes.";

const ENV_AFTER_HELP: &str = "\
Environment Variables:
  TEAMS_TOKEN_FILE=<PATH>               Load an external token file (JWT or JSON) directly (skips OS keyring)
  TEAMS_TOKEN_REFRESH_CMD=<CMD>         Command hook to run automatically when the token is about to expire
  TEAMS_TOKEN_REFRESH_MINS=<MINS>       Minutes remaining before triggering refresh hook [default: 5]
  TEAMS_ACCESS_TOKEN=<JWT>              Inject a Bearer access token directly (skips OS keyring)
  TEAMS_DISABLE_KEYRING=1               Disable the OS keyring (use credentials.json file only)
  TEAMS_CLIENT_ID / TEAMS_TENANT_ID     Override Entra app Client ID / Tenant ID
  TEAMS_CONFIG_DIR / TEAMS_DATA_DIR     Override config and local SQLite cache directories
  TEAMS_CLI_CACHE_SECS=<SECS>           Local cache TTL in seconds for CLI read commands [default: 0; --cached/--fast prefers cache]
  TEAMS_AUTO_DAEMON=1                   Auto-start/connect to background sync daemon (config.toml: auto_daemon = true) [default: false]
  TEAMS_IMAGE_PROTOCOL=<PROTO>          Terminal image protocol (auto, kitty, iterm2, sixel, halfblock)

Background Sync Daemon:
  teams daemon start | status | stop | restart";

#[derive(Parser, Debug)]
#[command(
    name = "teams",
    version,
    about = "Microsoft Teams terminal client (CLI · TUI · HTTP API · MCP)",
    long_about = "Terminal client for Microsoft Teams powered by Microsoft Graph v1.0.\n\
                  Running without a subcommand checks authentication and launches the TUI.",
    after_help = ENV_AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Sign in with Microsoft Entra ID (OAuth 2.0) and store tokens
    #[command(visible_alias = "in")]
    Login {
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        tenant_id: Option<String>,
        #[arg(long)]
        device_code: bool,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        json: bool,
    },
    /// Remove stored authentication tokens
    #[command(visible_alias = "out")]
    Logout {
        #[arg(long)]
        json: bool,
    },
    /// Display the currently signed-in user profile and token status
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Show config/DB/keyring paths and optional Graph connectivity diagnostics
    Doctor {
        /// Probe live connectivity via Graph `/me` and `/me/presence`
        #[arg(long)]
        probe: bool,
        #[arg(long)]
        json: bool,
    },
    /// List 1:1, group, and meeting chats
    #[command(visible_alias = "ls")]
    Chats {
        /// Maximum number of chats to fetch [default: 50]
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        /// Read from local SQLite cache first (skips network requests)
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new 1:1 or group chat
    #[command(name = "new-chat")]
    NewChat {
        /// User email or Entra user ID to invite (repeatable)
        #[arg(long = "to", required = true, value_name = "email_or_id")]
        to: Vec<String>,
        #[arg(long)]
        topic: Option<String>,
        #[arg(long)]
        message: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Change the topic of an existing group chat
    #[command(after_help = "Requires delegated Chat.ReadWrite and access to the group chat.")]
    UpdateChat {
        target: String,
        /// New group chat topic (up to 250 characters, without colons)
        #[arg(long)]
        topic: String,
        #[arg(long)]
        json: bool,
    },
    /// List joined teams and channels
    Teams {
        #[arg(long)]
        refresh: bool,
        /// Read from local SQLite cache first (skips network requests)
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// List channels for a specific team
    Channels {
        team_query: String,
        #[arg(long)]
        refresh: bool,
        /// Read from local SQLite cache first (skips network requests)
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read recent messages from a chat or channel
    #[command(visible_alias = "cat")]
    Read {
        target: String,
        /// Maximum number of messages, or replies when --thread includes the root separately
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        /// Fetch messages older than this timestamp (ISO-8601 / YYYY-MM-DD) or message ID
        #[arg(long, value_name = "TIMESTAMP_OR_MSG_ID")]
        before: Option<String>,
        /// Read a specific channel thread by root message ID
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        offline: bool,
        /// Read from local SQLite cache first (falls back to network if empty or insufficient)
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
        /// Return message IDs, authors, timestamps, plain text, reply links, and attachment IDs/names
        #[arg(long, requires = "json")]
        compact: bool,
    },
    /// Read a team channel root post and its thread replies
    Thread {
        channel_target: String,
        root_msg_id: String,
        /// Maximum number of replies (the root is included separately)
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        /// Read a previous thread fetch from the local cache
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
        /// Return message IDs, authors, timestamps, plain text, reply links, and attachment IDs/names
        #[arg(long, requires = "json")]
        compact: bool,
    },
    /// Send a Markdown message
    #[command(visible_alias = "msg")]
    Send {
        target: String,
        markdown: String,
        #[arg(long)]
        subject: Option<String>,
        /// Add a mention: <user_id:DisplayName> (repeatable)
        #[arg(long = "mention", value_name = "user_id:DisplayName")]
        mention: Vec<String>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        json: bool,
    },
    /// Reply to a channel thread or quote-reply to a chat message
    Reply {
        target: String,
        msg_id: String,
        markdown: String,
        #[arg(long = "mention", value_name = "user_id:DisplayName")]
        mention: Vec<String>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        json: bool,
    },
    /// Upload and send a file via OneDrive/SharePoint (or inline image with `--inline`)
    #[command(name = "send-file", visible_alias = "file")]
    SendFile {
        target: String,
        path: PathBuf,
        #[arg(long)]
        caption: Option<String>,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        inline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Download a message attachment
    #[command(visible_alias = "dl")]
    Download {
        target: String,
        msg_id: String,
        #[arg(long, default_value_t = 0)]
        index: usize,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Set or remove a reaction (like, heart, laugh, surprised, sad, angry) on a message
    React {
        target: String,
        msg_id: String,
        reaction: String,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Edit a message you sent
    #[command(after_help = "Channel edits require delegated ChannelMessage.ReadWrite (or legacy Group.ReadWrite.All). Chat edits require delegated Chat.ReadWrite. Graph reports permission and policy denials as errors.")]
    Edit {
        target: String,
        msg_id: String,
        /// Replace the entire message body with this Markdown
        #[arg(required_unless_present_any = ["find", "subject"], conflicts_with_all = ["find", "replace", "subject"])]
        new_markdown: Option<String>,
        /// Find one exact phrase in a single editable text node in the current body
        #[arg(long, requires = "replace")]
        find: Option<String>,
        /// Replace the phrase with literal text and preserve the surrounding HTML
        #[arg(long, requires = "find")]
        replace: Option<String>,
        /// Set the subject of a channel root post you sent
        #[arg(long, conflicts_with_all = ["new_markdown", "find", "replace", "thread"])]
        subject: Option<String>,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Delete a message you sent
    Delete {
        target: String,
        msg_id: String,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// List members of a chat or channel
    Members {
        target: String,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search messages in the local SQLite FTS5 cache
    Search {
        query: String,
        #[arg(long)]
        target: Option<String>,
        /// Maximum number of results to return [default: 50]
        #[arg(long, visible_alias = "top", default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Sync chats, teams, channels, and message history to the local SQLite cache
    Sync {
        /// Deep-sync only a specific conversation or channel
        #[arg(long)]
        target: Option<String>,
        /// Number of recent chats to sync [default: 100]
        #[arg(long, default_value_t = 100)]
        chats: usize,
        /// Number of messages per conversation/channel to backfill into local FTS5 cache [default: 50]
        #[arg(long, visible_aliases = ["limit", "top"], default_value_t = 50)]
        depth: usize,
        #[arg(long)]
        json: bool,
    },
    /// Export a conversation to Markdown, JSON, or plain text
    Export {
        target: String,
        #[arg(long, default_value = "md", value_parser = ["md", "json", "txt"])]
        format: String,
        #[arg(long)]
        include_threads: bool,
        /// Maximum number of messages to export [default: 200]
        #[arg(long, visible_alias = "top", default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Get or set your presence status
    Presence {
        #[arg(long, value_name = "Available|Busy|DoNotDisturb|BeRightBack|Away|Reset")]
        set: Option<String>,
        #[arg(long)]
        activity: Option<String>,
        #[arg(long)]
        expiration_mins: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Render a message image attachment or local image in the terminal
    Preview {
        target: Option<String>,
        msg_id: Option<String>,
        #[arg(long)]
        attachment: Option<String>,
        #[arg(long)]
        thread: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        /// Image rendering protocol override (auto, kitty, iterm2, sixel, halfblock)
        #[arg(long, value_name = "PROTOCOL")]
        protocol: Option<String>,
    },
    /// Render a local image file in the terminal
    #[command(name = "preview-file")]
    PreviewFile {
        path: PathBuf,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        /// Image rendering protocol override (auto, kitty, iterm2, sixel, halfblock)
        #[arg(long, value_name = "PROTOCOL")]
        protocol: Option<String>,
    },
    /// Check for or apply updates from GitHub Releases
    Update {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Launch the interactive terminal UI (TUI)
    #[command(visible_alias = "chat")]
    Tui,
    /// Start the local HTTP JSON API server for agents
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 8788)]
        port: u16,
        #[arg(long)]
        token: Option<String>,
    },
    /// Start the stdio MCP (JSON-RPC 2.0) server for agents
    #[command(visible_alias = "mcp-server")]
    Mcp,
    /// Control the background sync daemon (`start` | `stop` | `status` | `restart` | `run`)
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Start the background sync daemon (returns status if already running)
    Start {
        #[arg(long)]
        json: bool,
    },
    /// Stop the running background sync daemon
    Stop {
        #[arg(long)]
        json: bool,
    },
    /// Query the background sync daemon status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Restart the background sync daemon
    Restart {
        #[arg(long)]
        json: bool,
    },
    /// Run the background sync daemon in the foreground (internal use)
    Run,
}


/// Synchronously handles `--help`, `-h`, `--version`, `-V`, and `help` before Tokio/rustls/Keyring/SQLite init.
pub fn try_fast_help_or_version() -> Option<Result<()>> {
    try_fast_help_or_version_from(std::env::args_os(), Persona::detect())
}

fn try_fast_help_or_version_from<I, S>(args: I, persona: Persona) -> Option<Result<()>>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString> + Clone,
{
    let raw: Vec<std::ffi::OsString> = args.into_iter().map(Into::into).collect();
    let mut after_bin = raw.iter().skip(1);
    let first = after_bin.next()?;
    let has_help_or_version = first.to_str() == Some("help")
        || std::iter::once(first)
            .chain(after_bin)
            .take_while(|arg| arg.to_str() != Some("--"))
            .any(|arg| matches!(arg.to_str(), Some("--help" | "-h" | "--version" | "-V")));

    if !has_help_or_version {
        return None;
    }

    let err = match persona {
        Persona::Teams => Cli::try_parse_from(&raw).err()?,
    };
    if matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
    ) {
        let _ = err.print();
        return Some(Ok(()));
    }
    None
}

pub async fn run_from_env() -> Result<()> {
    let persona = Persona::detect();
    let cli = Cli::parse();
    let mut cfg = AppConfig::load();
    Box::pin(dispatch_command(&mut cfg, persona, cli.command)).await
}

async fn dispatch_command(
    cfg: &mut AppConfig,
    persona: Persona,
    command: Option<Command>,
) -> Result<()> {
    match command {
        None => Box::pin(cmd_tui(cfg, persona)).await,
        Some(Command::Login {
            client_id,
            tenant_id,
            device_code,
            client_secret,
            token,
            port,
            json,
        }) => {
            Box::pin(cmd_login(
                cfg,
                LoginOptions {
                    client_id,
                    tenant_id,
                    device_code,
                    client_secret,
                    access_token: token,
                    port,
                },
                json,
            ))
            .await
        }
        Some(Command::Logout { json }) => Box::pin(cmd_logout(cfg, json)).await,
        Some(Command::Whoami { json }) => Box::pin(cmd_whoami(cfg, persona, json)).await,
        Some(Command::Doctor { probe, json }) => Box::pin(cmd_doctor(cfg, persona, probe, json)).await,
        Some(Command::Chats {
            limit,
            refresh,
            cached,
            json,
        }) => Box::pin(cmd_chats(cfg, limit, refresh, cached, json)).await,
        Some(Command::NewChat {
            to,
            topic,
            message,
            json,
        }) => Box::pin(cmd_new_chat(cfg, to, topic, message, json)).await,
        Some(Command::UpdateChat { target, topic, json }) => {
            Box::pin(cmd_update_chat(cfg, &target, &topic, json)).await
        }
        Some(Command::Teams {
            refresh,
            cached,
            json,
        }) => Box::pin(cmd_teams(cfg, refresh, cached, json)).await,
        Some(Command::Channels {
            team_query,
            refresh,
            cached,
            json,
        }) => Box::pin(cmd_channels(cfg, &team_query, refresh, cached, json)).await,
        Some(Command::Read {
            target,
            limit,
            before,
            thread,
            offline,
            cached,
            json,
            compact,
        }) => {
            Box::pin(cmd_read(
                cfg,
                &target,
                limit,
                before.as_deref(),
                thread.as_deref(),
                offline,
                cached,
                ReadOutput { json, compact },
            ))
            .await
        }
        Some(Command::Thread {
            channel_target,
            root_msg_id,
            limit,
            offline,
            json,
            compact,
        }) => {
            Box::pin(cmd_thread_with_cache(
                cfg,
                &channel_target,
                &root_msg_id,
                limit,
                offline,
                false,
                ReadOutput { json, compact },
            ))
            .await
        }
        Some(Command::Send {
            target,
            markdown,
            subject,
            mention,
            important,
            json,
        }) => {
            Box::pin(cmd_send(
                cfg,
                &target,
                &markdown,
                subject,
                &mention,
                important,
                json,
            ))
            .await
        }
        Some(Command::Reply {
            target,
            msg_id,
            markdown,
            mention,
            important,
            json,
        }) => {
            Box::pin(cmd_reply(
                cfg,
                &target,
                &msg_id,
                &markdown,
                &mention,
                important,
                json,
            ))
            .await
        }
        Some(Command::SendFile {
            target,
            path,
            caption,
            thread,
            inline,
            json,
        }) => {
            Box::pin(cmd_send_file(
                cfg,
                &target,
                &path,
                caption.as_deref(),
                thread.as_deref(),
                inline,
                json,
            ))
            .await
        }
        Some(Command::Download {
            target,
            msg_id,
            index,
            thread,
            out,
            json,
        }) => {
            Box::pin(cmd_download(
                cfg,
                &target,
                &msg_id,
                index,
                thread.as_deref(),
                out.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::React {
            target,
            msg_id,
            reaction,
            off,
            thread,
            json,
        }) => {
            Box::pin(cmd_react(
                cfg,
                &target,
                &msg_id,
                &reaction,
                !off,
                thread.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Edit {
            target,
            msg_id,
            new_markdown,
            find,
            replace,
            subject,
            thread,
            json,
        }) => {
            let edit = MessageEdit::from_args(
                new_markdown.as_deref(),
                find.as_deref(),
                replace.as_deref(),
                subject.as_deref(),
            )?;
            Box::pin(cmd_edit(
                cfg,
                &target,
                &msg_id,
                edit,
                thread.as_deref(),
                json,
            ))
            .await
        }
        Some(Command::Delete {
            target,
            msg_id,
            thread,
            yes,
            json,
        }) => Box::pin(cmd_delete(cfg, &target, &msg_id, thread.as_deref(), yes, json)).await,
        Some(Command::Members {
            target,
            offline,
            json,
        }) => Box::pin(cmd_members(cfg, &target, offline, json)).await,
        Some(Command::Search {
            query,
            target,
            limit,
            json,
        }) => Box::pin(cmd_search(cfg, &query, target.as_deref(), limit, json)).await,
        Some(Command::Sync {
            target,
            chats,
            depth,
            json,
        }) => Box::pin(cmd_sync(cfg, target.as_deref(), chats, depth, json)).await,
        Some(Command::Export {
            target,
            format,
            include_threads,
            limit,
            offline,
            out,
        }) => {
            Box::pin(cmd_export(
                cfg,
                &target,
                &format,
                include_threads,
                limit,
                offline,
                out.as_deref(),
            ))
            .await
        }
        Some(Command::Presence {
            set,
            activity,
            expiration_mins,
            json,
        }) => {
            Box::pin(cmd_presence(
                cfg,
                set.as_deref(),
                activity.as_deref(),
                expiration_mins,
                json,
            ))
            .await
        }
        Some(Command::Preview {
            target,
            msg_id,
            attachment,
            thread,
            file,
            cols,
            rows,
            protocol,
        }) => {
            Box::pin(cmd_preview_teams(
                cfg,
                target.as_deref(),
                msg_id.as_deref(),
                attachment.as_deref(),
                thread.as_deref(),
                file.as_deref(),
                cols,
                rows,
                protocol.as_deref(),
            ))
            .await
        }
        Some(Command::PreviewFile {
            path,
            cols,
            rows,
            protocol,
        }) => cmd_preview_local_file(cfg, &path, cols, rows, protocol.as_deref()),
        Some(Command::Update {
            check,
            force,
            repo,
            json,
        }) => Box::pin(cmd_update(cfg, check, force, repo.as_deref(), json)).await,
        Some(Command::Tui) => Box::pin(cmd_tui(cfg, persona)).await,
        Some(Command::Serve { bind, port, token }) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(serve::run(
                cfg.clone(),
                graph,
                storage,
                ServeOptions { bind, port, token },
            ))
            .await
        }
        Some(Command::Mcp) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(crate::mcp::run(cfg.clone(), graph, storage)).await
        }
        Some(Command::Daemon { command }) => Box::pin(dispatch_daemon_command(cfg, command)).await,
    }
}


fn print_daemon_status_text(header: &str, cfg: &AppConfig, status: &crate::daemon::DaemonStatus) {
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

async fn dispatch_daemon_command(cfg: &AppConfig, command: DaemonCommand) -> Result<()> {
    match command {
        DaemonCommand::Start { json } => {
            let status = crate::daemon::ensure_daemon_started(cfg).await?;
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
            let status = crate::daemon::ensure_daemon_started(cfg).await?;
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


async fn open_runtime(
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

fn db_path_in_dir(data_dir: &Path, user_id: Option<&str>) -> PathBuf {
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

fn extract_user_id_from_jwt(raw_token: &str) -> Option<String> {
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

fn extract_user_id_from_token_file(path: &Path) -> Option<String> {
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

fn find_single_existing_db(data_dir: &Path) -> Option<PathBuf> {
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

fn resolve_storage_db_path_fast(
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

fn infer_user_id_fast_with(
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

async fn resolve_daemon_warm(cfg: &AppConfig, refresh: bool, cached: bool) -> bool {
    if refresh || cached {
        return false;
    }
    let env_tok = std::env::var("TEAMS_ACCESS_TOKEN").ok();
    let cred_path = config::credential_fallback_path();
    let inferred_user_id = infer_user_id_fast_with(cfg, env_tok.as_deref(), &cred_path);
    let daemon_warm = crate::daemon::is_daemon_alive_fast(cfg, inferred_user_id.as_deref());
    if cfg.auto_daemon && !daemon_warm {
        let _ = crate::daemon::ensure_daemon_started(cfg).await;
    }
    daemon_warm
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
fn parse_cli_cache_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0)
}

fn cli_cache_ttl_secs(cfg: &AppConfig) -> u64 {
    if let Ok(raw) = std::env::var("TEAMS_CLI_CACHE_SECS") {
        if let Ok(parsed) = raw.trim().parse::<u64>() {
            return parsed;
        }
    }
    cfg.cli_cache_ttl_secs
}

fn is_cursor_fresh(storage: &Storage, scope_keys: &[&str], ttl_secs: u64) -> bool {
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

fn record_cli_sync_cursor(storage: &Storage, scope_key: &str) {
    let cursor = crate::models::SyncCursor {
        scope_key: scope_key.to_string(),
        delta_link: None,
        oldest_msg_ts: None,
        synced_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = storage.save_sync_cursor(&cursor);
}

async fn cmd_tui(cfg: &mut AppConfig, persona: Persona) -> Result<()> {
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
    if storage.list_conversations()?.is_empty() {
        let _ = sync::bootstrap_conversations(&graph, &storage).await;
    }
    tui::run(cfg.clone(), auth, graph, storage).await
}

async fn cmd_login(cfg: &mut AppConfig, opts: LoginOptions, json_out: bool) -> Result<()> {
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

async fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    auth.logout().await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("Signed out and removed stored tokens.");
    }
    Ok(())
}

async fn cmd_whoami(cfg: &AppConfig, persona: Persona, json_out: bool) -> Result<()> {
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
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
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
                }))?
            );
            bail!("Token expired (exp: {}) — refresh the token and retry.", format_unix_ts(tokens.expires_at));
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
            display_name.as_deref().unwrap_or("-"),
            email.as_deref().unwrap_or("-")
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

async fn cmd_doctor(
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

    let mut probe_me: Option<serde_json::Value> = None;
    let mut probe_presence: Option<serde_json::Value> = None;
    let mut probe_error: Option<String> = None;

    if probe {
        let mut graph = GraphClient::new(cfg.clone(), auth.clone())?;
        if let Some(uid) = user_id {
            graph = graph.with_my_user_id(uid);
        }
        match graph.get_me().await {
            Ok(me) => {
                probe_me = Some(serde_json::to_value(&me)?);
                match graph.get_my_presence().await {
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

fn describe_keyring_status(tokens: Option<&StoredTokens>, fallback_path: &Path) -> String {
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

fn print_chats_output(chats: &[ConversationSummary], json_out: bool) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "chats": chats }))?
        );
    } else if chats.is_empty() {
        println!("No chats to display.");
    } else {
        for c in chats {
            let unread = if c.unread_count > 0 {
                format!(" [unread:{}]", c.unread_count)
            } else {
                String::new()
            };
            let preview = c.last_msg_preview.as_deref().unwrap_or("");
            println!(
                "{}  [{}] {}{}  {}",
                c.conv_key,
                c.kind.as_str(),
                c.title,
                unread,
                truncate_single_line(preview, 60)
            );
        }
    }
    Ok(())
}

async fn cmd_chats(
    cfg: &AppConfig,
    limit: usize,
    refresh: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let limit = clamp_limit(limit);
    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:chats"], ttl) {
                let mut chats: Vec<ConversationSummary> = storage
                    .list_conversations()?
                    .into_iter()
                    .filter(|c| !c.kind.is_channel())
                    .collect();
                if !chats.is_empty() && (cached || chats.len() >= limit) {
                    chats.truncate(limit);
                    return print_chats_output(&chats, json_out);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;

    let cached_chats: Vec<ConversationSummary> = storage
        .list_conversations()?
        .into_iter()
        .filter(|c| !c.kind.is_channel())
        .collect();

    let cache_usable = !refresh
        && (cached || daemon_warm || (ttl > 0 && is_cursor_fresh(&storage, &["cli:chats"], ttl)));
    let mut chats = if !cache_usable || (!cached && cached_chats.len() < limit) || cached_chats.is_empty() {
        let fetched = match graph.list_chats(limit).await {
            Ok(items) => items,
            Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
        };
        storage.upsert_conversations(&fetched)?;
        record_cli_sync_cursor(&storage, "cli:chats");
        let stored: Vec<ConversationSummary> = storage
            .list_conversations()?
            .into_iter()
            .filter(|c| !c.kind.is_channel())
            .collect();
        if stored.is_empty() {
            fetched
        } else {
            stored
        }
    } else {
        cached_chats
    };

    chats.truncate(limit);
    print_chats_output(&chats, json_out)
}

async fn cmd_new_chat(
    cfg: &AppConfig,
    to: Vec<String>,
    topic: Option<String>,
    message: Option<String>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let members: Vec<String> = to
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if members.is_empty() {
        bail!("At least one recipient (--to) must be specified.");
    }

    let chat = graph.create_chat(&members, topic.as_deref()).await?;
    storage.upsert_conversations(std::slice::from_ref(&chat))?;

    let mut sent_msg: Option<MessageItem> = None;
    if let Some(md) = message {
        if !md.trim().is_empty() {
            let outgoing =
                parser::compile_outgoing_markdown(&md, None, vec![], vec![], vec![], None);
            let sent = graph.send_message(&chat.target, &outgoing).await?;
            let _ = storage.upsert_messages(std::slice::from_ref(&sent));
            sent_msg = Some(sent);
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "chat": chat,
                "message": sent_msg,
            }))?
        );
    } else {
        println!("Chat created: {} ({})", chat.title, chat.conv_key);
        if let Some(msg) = sent_msg {
            println!("Initial message sent: msg_id={}", msg.msg_id);
        }
    }
    Ok(())
}

fn print_teams_output(teams: &[TeamSummary], json_out: bool) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "teams": teams }))?
        );
    } else if teams.is_empty() {
        println!("No joined teams found.");
    } else {
        for team in teams {
            println!("{} ({})", team.display_name, team.id);
            for ch in &team.channels {
                println!(
                    "  - {} [{}] ({})",
                    ch.display_name,
                    ch.membership_type,
                    ch.conv_key()
                );
            }
        }
    }
    Ok(())
}

fn print_channels_output(
    team: &TeamSummary,
    channels: &[crate::models::ChannelSummary],
    json_out: bool,
) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "team": { "id": team.id, "display_name": team.display_name },
                "channels": channels,
            }))?
        );
    } else {
        println!("# {} ({})", team.display_name, team.id);
        for ch in channels {
            println!(
                "  {}  [{}]  {}",
                ch.conv_key(),
                ch.membership_type,
                ch.display_name
            );
        }
    }
    Ok(())
}

async fn cmd_teams(cfg: &AppConfig, refresh: bool, cached: bool, json_out: bool) -> Result<()> {
    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:teams"], ttl) {
                let teams = storage.list_teams()?;
                if !teams.is_empty() {
                    return print_teams_output(&teams, json_out);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let force_fetch = refresh
        || (!cached
            && !daemon_warm
            && !(ttl > 0 && is_cursor_fresh(&storage, &["cli:teams"], ttl)));
    let teams = load_teams_with_refresh(&graph, &storage, force_fetch).await?;
    print_teams_output(&teams, json_out)
}

async fn cmd_channels(
    cfg: &AppConfig,
    team_query: &str,
    refresh: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:teams"], ttl) {
                let teams = storage.list_teams()?;
                if !teams.is_empty() {
                    if let Ok(team) = resolve_team_from_slice(&teams, team_query) {
                        return print_channels_output(&team, &team.channels, json_out);
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let force_fetch = refresh
        || (!cached
            && !daemon_warm
            && !(ttl > 0 && is_cursor_fresh(&storage, &["cli:teams"], ttl)));
    let teams = load_teams_with_refresh(&graph, &storage, force_fetch).await?;
    let team = resolve_team_from_slice(&teams, team_query)?;

    let channels = if refresh && team.channels.is_empty() {
        graph.list_team_channels(&team.id, &team.display_name).await?
    } else {
        team.channels.clone()
    };

    print_channels_output(&team, &channels, json_out)
}

async fn load_teams_with_refresh(
    graph: &GraphClient,
    storage: &Storage,
    refresh: bool,
) -> Result<Vec<TeamSummary>> {
    let cached = storage.list_teams()?;
    if refresh || cached.is_empty() {
        let fetched = graph.list_joined_teams_with_channels().await?;
        storage.upsert_teams(&fetched)?;
        record_cli_sync_cursor(storage, "cli:teams");
        let stored = storage.list_teams()?;
        if stored.is_empty() {
            Ok(fetched)
        } else {
            Ok(stored)
        }
    } else {
        Ok(cached)
    }
}

fn resolve_team_from_slice(teams: &[TeamSummary], query: &str) -> Result<TeamSummary> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Team name or ID is required.");
    }
    if let Some(exact_id) = teams.iter().find(|t| t.id == q) {
        return Ok(exact_id.clone());
    }
    let q_lower = q.to_lowercase();
    let exact_name: Vec<&TeamSummary> = teams
        .iter()
        .filter(|t| t.display_name.to_lowercase() == q_lower)
        .collect();
    if exact_name.len() == 1 {
        return Ok(exact_name[0].clone());
    }
    let matches: Vec<&TeamSummary> = teams
        .iter()
        .filter(|t| t.display_name.to_lowercase().contains(&q_lower))
        .collect();
    match matches.len() {
        0 => bail!("Team not found: {q}"),
        1 => Ok(matches[0].clone()),
        _ => {
            let names: Vec<String> = matches
                .iter()
                .map(|t| format!("{} ({})", t.display_name, t.id))
                .collect();
            bail!("Multiple teams match ({q}): {}", names.join(", "))
        }
    }
}

#[derive(Clone, Copy)]
struct ReadOutput {
    json: bool,
    compact: bool,
}

fn print_read_output(
    conv: &ConversationSummary,
    res: &ReadMessagesResult,
    output: ReadOutput,
) -> Result<()> {
    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&read_messages_json(conv, res, output.compact))?
        );
    } else {
        println!("# {} ({})", conv.title, conv.conv_key);
        if res.messages.is_empty() {
            println!("(No messages)");
        } else {
            for msg in &res.messages {
                print_message_line(msg, "");
            }
            if res.has_more {
                if let Some(ref next_before) = res.next_before {
                    println!(
                        "--- Load older messages: teams read \"{}\" --before {} ---",
                        conv.conv_key, next_before
                    );
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_read(
    cfg: &AppConfig,
    target: &str,
    limit: usize,
    before: Option<&str>,
    thread: Option<&str>,
    offline: bool,
    cached: bool,
    output: ReadOutput,
) -> Result<()> {
    if let Some(root_msg_id) = thread.map(str::trim).filter(|s| !s.is_empty()) {
        return cmd_thread_with_cache(cfg, target, root_msg_id, limit, offline, cached, output)
            .await;
    }

    let limit = clamp_limit(limit);
    if offline {
        let storage = open_storage_only(cfg)?;
        let cached_convs = storage.list_conversations()?;
        let conv = resolver::resolve_from_slice(&cached_convs, target)?;
        let res = read_conversation_messages_offline(&storage, &conv, limit, before)?;
        return print_read_output(&conv, &res, output);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    if cached || (before.is_none() && ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(cached_convs) = storage.list_conversations() {
                if let Ok(conv) = resolver::resolve_from_slice(&cached_convs, target) {
                    if before.is_some() {
                        let res = read_conversation_messages_offline(&storage, &conv, limit, before)?;
                        let is_eof = storage
                            .get_sync_cursor(&SyncCursor::history_scope(&conv.conv_key))
                            .ok()
                            .flatten()
                            .is_some_and(|c| c.is_history_eof());
                        if res.messages.len() >= limit || is_eof {
                            return print_read_output(&conv, &res, output);
                        }
                    } else {
                        let cursor_key = format!("cli:read:{}", conv.conv_key);
                        if cached
                            || is_cursor_fresh(
                                &storage,
                                &[cursor_key.as_str(), conv.conv_key.as_str()],
                                ttl,
                            )
                        {
                            let res = read_conversation_messages_offline(&storage, &conv, limit, None)?;
                            if !res.messages.is_empty() && (cached || res.messages.len() >= limit) {
                                return print_read_output(&conv, &res, output);
                            }
                        }
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let res = read_conversation_messages(&storage, &graph, &conv, limit, before, false, true).await?;
    if before.is_none() {
        record_cli_sync_cursor(&storage, &format!("cli:read:{}", conv.conv_key));
    }

    print_read_output(&conv, &res, output)
}

async fn cmd_thread_with_cache(
    cfg: &AppConfig,
    channel_target: &str,
    root_msg_id: &str,
    limit: usize,
    offline: bool,
    cached: bool,
    output: ReadOutput,
) -> Result<()> {
    let limit = clamp_limit(limit);
    if offline {
        let storage = open_storage_only(cfg)?;
        let conv = resolver::resolve_from_slice(&storage.list_conversations()?, channel_target)?;
        let thread = read_thread_offline(&storage, &conv, root_msg_id, limit)?
            .ok_or_else(|| thread_cache_miss(root_msg_id, limit))?;
        return print_thread_output(&conv, &thread, output);
    }
    if cached {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(cached_convs) = storage.list_conversations() {
                if let Ok(conv) = resolver::resolve_from_slice(&cached_convs, channel_target) {
                    if let Some(thread) = read_thread_offline(&storage, &conv, root_msg_id, limit)? {
                        return print_thread_output(&conv, &thread, output);
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, channel_target, false).await?;
    let thread = match read_thread(&storage, &graph, &conv, root_msg_id, limit).await {
        Ok(thread) => thread,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    print_thread_output(&conv, &thread, output)
}

fn print_thread_output(
    conv: &ConversationSummary,
    thread: &ThreadView,
    output: ReadOutput,
) -> Result<()> {
    if output.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&thread_json(conv, thread, output.compact))?
        );
    } else {
        println!("# {} ({}) — Thread {}", conv.title, conv.conv_key, thread.root.msg_id);
        print_message_line(&thread.root, "");
        if thread.replies.is_empty() {
            println!("  (No replies)");
        } else {
            for reply in &thread.replies {
                print_message_line(reply, "  ↳ ");
            }
        }
    }
    Ok(())
}

fn format_message_line(msg: &MessageItem, indent: &str) -> String {
    let attachments = msg.effective_attachments();
    let mut tags = Vec::new();
    if let Some(ref subj) = msg.subject {
        if !subj.trim().is_empty() {
            tags.push(format!("[Subject: {}]", subj.trim()));
        }
    }
    if msg.importance.eq_ignore_ascii_case("high") || msg.importance.eq_ignore_ascii_case("urgent") {
        tags.push(format!("[Importance:{}]", msg.importance));
    }
    if msg.reply_count > 0 {
        tags.push(format!("[💬 {}]", msg.reply_count));
    }
    if !attachments.is_empty() {
        let names: Vec<String> = attachments
            .iter()
            .map(|a| {
                a.name
                    .clone()
                    .unwrap_or_else(|| a.content_type.clone())
            })
            .collect();
        tags.push(format!("[📎 {}]", names.join(", ")));
    }
    if !msg.reactions.is_empty() {
        let reacts: Vec<String> = msg
            .reactions
            .iter()
            .map(|r| format!("{} {}", reaction_symbol(&r.reaction_type), r.count))
            .collect();
        tags.push(format!("[{}]", reacts.join(" ")));
    }

    let body = if msg.deleted {
        "(deleted message)".to_string()
    } else if !msg.body_plain.trim().is_empty() {
        msg.body_plain.trim().to_string()
    } else {
        parser::html_and_cards_to_plain(&msg.body_html, &attachments)
            .trim()
            .to_string()
    };

    let quote = match (&msg.reply_to_author, &msg.reply_to_preview) {
        (Some(author), Some(prev)) => format!(" ↩ {author}: {}", truncate_single_line(prev, 40)),
        _ => String::new(),
    };

    let tag_str = if tags.is_empty() {
        String::new()
    } else {
        format!(" {}", tags.join(" "))
    };

    format!(
        "{indent}[{}] {} ({}){quote}{tag_str}: {}",
        msg.created_at, msg.author_name, msg.msg_id, body
    )
}

fn print_message_line(msg: &MessageItem, indent: &str) {
    println!("{}", format_message_line(msg, indent));
}

fn reaction_symbol(reaction_type: &str) -> &str {
    match reaction_type {
        "like" => "👍",
        "heart" => "❤️",
        "laugh" => "😄",
        "surprised" => "😮",
        "sad" => "😢",
        "angry" => "😡",
        other => other,
    }
}

async fn cmd_send(
    cfg: &AppConfig,
    target: &str,
    markdown: &str,
    subject: Option<String>,
    mentions_raw: &[String],
    important: bool,
    json_out: bool,
) -> Result<()> {
    if markdown.trim().is_empty() {
        bail!("Message body is empty.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let mentions = parse_mentions_cli(mentions_raw)?;

    let mut outgoing =
        parser::compile_outgoing_markdown(markdown, subject, mentions, vec![], vec![], None);
    if important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = graph.send_message(&conv.target, &outgoing).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&sent));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conversation": conv,
                "message": sent,
            }))?
        );
    } else {
        println!("Sent: {} ({}) msg_id={}", conv.title, conv.conv_key, sent.msg_id);
    }
    Ok(())
}

async fn cmd_reply(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    markdown: &str,
    mentions_raw: &[String],
    important: bool,
    json_out: bool,
) -> Result<()> {
    if markdown.trim().is_empty() {
        bail!("Reply body is empty.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let mentions = parse_mentions_cli(mentions_raw)?;

    let quote = build_quote_reply(&storage, &graph, &conv, msg_id).await;
    let mut outgoing =
        parser::compile_outgoing_markdown(markdown, None, mentions, vec![], vec![], quote);
    if important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = graph.reply_message(&conv.target, msg_id, &outgoing).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&sent));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conversation": conv,
                "reply_to": msg_id,
                "message": sent,
            }))?
        );
    } else {
        println!(
            "Replied: {} ({}) msg_id={} (to {})",
            conv.title, conv.conv_key, sent.msg_id, msg_id
        );
    }
    Ok(())
}

async fn cmd_send_file(
    cfg: &AppConfig,
    target: &str,
    path: &Path,
    caption: Option<&str>,
    thread: Option<&str>,
    inline: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let sent = send_file_or_inline(&graph, &storage, &conv, path, caption, thread, inline).await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conversation": conv,
                "path": path,
                "inline": inline,
                "message": sent,
            }))?
        );
    } else {
        println!(
            "File sent: {} ({}) msg_id={} ({})",
            conv.title,
            conv.conv_key,
            sent.msg_id,
            path.display()
        );
    }
    Ok(())
}

async fn cmd_download(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    index: usize,
    thread: Option<&str>,
    out: Option<&Path>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let out_dir = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded =
        download_message_attachment(&graph, &storage, &conv, msg_id, index, thread, &out_dir)
            .await?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "path": downloaded.path,
                "file_name": downloaded.file_name,
                "mime_type": downloaded.mime_type,
                "size_bytes": downloaded.bytes_written,
                "bytes_written": downloaded.bytes_written,
                "is_image": downloaded.is_image,
            }))?
        );
    } else {
        println!(
            "Downloaded: {} ({} bytes)",
            downloaded.path.display(),
            downloaded.bytes_written
        );
    }
    Ok(())
}

async fn cmd_react(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    reaction: &str,
    enable: bool,
    thread: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let normalized = normalize_reaction(reaction)?;
    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    graph
        .set_reaction(&conv.target, msg_id, thread, &normalized, enable)
        .await?;
    let my_name = auth
        .current_tokens()
        .await
        .and_then(|t| t.user_display_name);
    let _ = storage.apply_local_reaction(
        &conv.conv_key,
        msg_id,
        &normalized,
        enable,
        my_name.as_deref(),
    );

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
                "reaction": normalized,
                "enable": enable,
            }))?
        );
    } else {
        println!(
            "Reaction {}: {} ({}) msg_id={}",
            if enable { "set" } else { "removed" },
            normalized,
            conv.conv_key,
            msg_id
        );
    }
    Ok(())
}

async fn cmd_update_chat(
    cfg: &AppConfig,
    target: &str,
    topic: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let result = update_group_chat_topic(&storage, &graph, target, topic).await;
    let chat = match result {
        Ok(chat) => chat,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({
            "ok": true,
            "conversation": chat,
        }))?);
    } else {
        println!("Group chat topic updated: {} ({})", chat.title, chat.conv_key);
    }
    Ok(())
}

async fn cmd_edit(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    edit: MessageEdit<'_>,
    thread: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    if let Err(err) = edit_sent_message(&storage, &graph, &conv, msg_id, thread, edit).await {
        return Err(wrap_graph_permission_error(&graph, err).await);
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))?
        );
    } else {
        println!("Message edited: {} msg_id={}", conv.conv_key, msg_id);
    }
    Ok(())
}

async fn cmd_delete(
    cfg: &AppConfig,
    target: &str,
    msg_id: &str,
    thread: Option<&str>,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    if json_out && !yes {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;

    let answer = if yes {
        None
    } else {
        Some(prompt(&format!(
            "Delete message {msg_id} in '{}' ({})? Type 'yes' to continue: ",
            conv.title, conv.conv_key
        ))?)
    };
    delete_gate(yes, json_out, answer.as_deref())?;

    graph.delete_message(&conv.target, msg_id, thread).await?;
    let _ = storage.apply_local_soft_delete(&conv.conv_key, msg_id, &Utc::now().to_rfc3339());

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))?
        );
    } else {
        println!("Message deleted: {} msg_id={}", conv.conv_key, msg_id);
    }
    Ok(())
}

fn delete_gate(yes: bool, json: bool, answer: Option<&str>) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json {
        bail!("{DELETE_JSON_REQUIRES_YES}");
    }
    let answer = answer.context("No confirmation input received. Message was not deleted.")?;
    if !answer.trim().eq_ignore_ascii_case("yes") {
        bail!("Cancelled. Message was not deleted.");
    }
    Ok(())
}

async fn cmd_members(cfg: &AppConfig, target: &str, offline: bool, json_out: bool) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let conv = resolve_target(&storage, &graph, target, offline).await?;

    let members = if offline {
        storage.list_members(&conv.conv_key)?
    } else {
        let fetched = graph.list_members(&conv.target).await?;
        let _ = storage.upsert_members(&conv.conv_key, &fetched);
        fetched
    };

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "conv_key": conv.conv_key,
                "title": conv.title,
                "members": members,
            }))?
        );
    } else if members.is_empty() {
        println!("No member information available.");
    } else {
        println!("# {} ({}) Members ({})", conv.title, conv.conv_key, members.len());
        for m in &members {
            let role = if m.roles.is_empty() {
                String::new()
            } else {
                format!(" [{}]", m.roles.join(","))
            };
            let pres = m
                .presence
                .as_ref()
                .map(|p| format!(" ({})", p.availability))
                .unwrap_or_default();
            println!(
                "  {}  <{}>{}{}",
                m.display_name,
                m.email.as_deref().unwrap_or(&m.user_id),
                role,
                pres
            );
        }
    }
    Ok(())
}

async fn cmd_search(
    cfg: &AppConfig,
    query: &str,
    target: Option<&str>,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    if query.trim().is_empty() {
        bail!("Search query is empty.");
    }
    let storage = open_storage_only(cfg)?;
    let limit = clamp_limit(limit);

    let conv_key_filter = match target {
        Some(t) => {
            let cached = storage.list_conversations()?;
            Some(resolver::resolve_from_slice(&cached, t)?.conv_key)
        }
        None => None,
    };

    let hits = storage.search_messages(query, conv_key_filter.as_deref(), limit)?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "query": query,
                "hits": hits,
            }))?
        );
    } else if hits.is_empty() {
        println!("No messages matching '{query}'.");
    } else {
        for hit in &hits {
            println!(
                "[{}] {} | {}: {}",
                hit.created_at, hit.conv_title, hit.author_name, hit.snippet
            );
        }
    }
    Ok(())
}

async fn sync_conversation_depth(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    depth: usize,
) -> Result<usize> {
    let first_page_top = depth.min(50);
    let (latest_msgs, first_next_link) = match graph
        .list_messages_page(&conv.target, first_page_top, None)
        .await
    {
        Ok(res) => res,
        Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
    };
    let mut synced_count = if latest_msgs.is_empty() {
        0
    } else {
        storage.upsert_messages(&latest_msgs).unwrap_or(latest_msgs.len())
    };
    let mut fetched_roots = latest_msgs
        .iter()
        .filter(|m| m.parent_msg_id.is_none())
        .count();

    let scope_key = SyncCursor::history_scope(&conv.conv_key);
    let mut current_next_link = first_next_link;
    let mut oldest_seen_ts = latest_msgs
        .iter()
        .filter(|m| m.parent_msg_id.is_none())
        .map(|m| m.created_at.clone())
        .min();

    while fetched_roots < depth {
        let Some(link) = current_next_link.clone() else {
            break;
        };
        let page_top = (depth - fetched_roots).min(50);
        let (page_msgs, new_next_link) = match graph
            .list_messages_page(&conv.target, page_top, Some(&link))
            .await
        {
            Ok(res) => res,
            Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
        };
        if !page_msgs.is_empty() {
            synced_count += storage.upsert_messages(&page_msgs).unwrap_or(page_msgs.len());
            fetched_roots += page_msgs
                .iter()
                .filter(|m| m.parent_msg_id.is_none())
                .count();
            if let Some(page_oldest) = page_msgs
                .iter()
                .filter(|m| m.parent_msg_id.is_none())
                .map(|m| m.created_at.clone())
                .min()
            {
                oldest_seen_ts = match oldest_seen_ts {
                    Some(prev) if prev < page_oldest => Some(prev),
                    _ => Some(page_oldest),
                };
            }
        }
        let no_progress = page_msgs.is_empty() && new_next_link.as_deref() == Some(link.as_str());
        current_next_link = new_next_link;
        if current_next_link.is_none() || no_progress {
            break;
        }
    }

    let oldest_ts = if current_next_link.is_none() {
        Some(SyncCursor::HISTORY_EOF.to_string())
    } else {
        oldest_seen_ts
    };
    let _ = storage.save_sync_cursor(&SyncCursor {
        scope_key,
        delta_link: current_next_link,
        oldest_msg_ts: oldest_ts,
        synced_at: Utc::now().to_rfc3339(),
    });

    let local_count = storage
        .list_root_messages(&conv.conv_key, depth, None)
        .map(|v| v.len())
        .unwrap_or(0);

    Ok(synced_count.max(local_count))
}

async fn cmd_sync(
    cfg: &AppConfig,
    target: Option<&str>,
    chats: usize,
    depth: usize,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let chats_limit = clamp_limit(chats);
    let depth = clamp_limit(depth);

    let targets: Vec<ConversationSummary> = match target {
        Some(t) => vec![resolve_target(&storage, &graph, t, false).await?],
        None => {
            let (chats_res, teams_res) = tokio::join!(
                graph.list_chats(chats_limit),
                graph.list_joined_teams_with_channels(),
            );
            let fetched_chats = match chats_res {
                Ok(c) => c,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            storage.upsert_conversations(&fetched_chats)?;
            if let Ok(teams) = teams_res {
                storage.upsert_teams(&teams)?;
            }
            let all_convs = storage.list_conversations()?;
            let mut chat_count = 0usize;
            let mut selected = Vec::new();
            for conv in all_convs {
                if conv.kind.is_channel() {
                    selected.push(conv);
                } else if chat_count < chats_limit {
                    chat_count += 1;
                    selected.push(conv);
                }
            }
            selected
        }
    };

    if !json_out && target.is_some() {
        for conv in &targets {
            println!("Syncing: {} ({}, target depth {})...", conv.title, conv.conv_key, depth);
        }
    }

    let mut total_messages = 0usize;
    let mut synced_convs = 0usize;
    let mut errors = Vec::new();

    let results: Vec<(String, Result<usize>)> = stream::iter(targets.iter())
        .map(|conv| {
            let storage = &storage;
            let graph = &graph;
            async move {
                let res = sync_conversation_depth(storage, graph, conv, depth).await;
                (conv.conv_key.clone(), res)
            }
        })
        .buffer_unordered(6)
        .collect()
        .await;

    for (conv_key, res) in results {
        match res {
            Ok(count) => {
                total_messages += count;
                synced_convs += 1;
            }
            Err(err) => {
                errors.push(format!("{conv_key}: {err:#}"));
            }
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": errors.is_empty(),
                "chats_limit": chats_limit,
                "depth": depth,
                "conversations_total": targets.len(),
                "conversations_synced": synced_convs,
                "messages_synced": total_messages,
                "errors": errors,
            }))?
        );
    } else {
        println!(
            "Sync complete: {synced_convs}/{} conversations · {total_messages} messages (chats={}, depth={})",
            targets.len(),
            chats_limit,
            depth
        );
        for err in &errors {
            eprintln!("Warning: {err}");
        }
    }
    Ok(())
}

async fn cmd_export(
    cfg: &AppConfig,
    target: &str,
    format: &str,
    include_threads: bool,
    limit: usize,
    offline: bool,
    out: Option<&Path>,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, !offline).await?;
    let limit = clamp_limit(limit);
    let conv = resolve_target(&storage, &graph, target, offline).await?;

    let fetched = if !offline {
        let msgs = graph.list_messages(&conv.target, limit).await?;
        storage.upsert_messages(&msgs)?;
        Some(msgs)
    } else {
        None
    };

    let mut messages = storage.list_root_messages(&conv.conv_key, limit, None)?;
    if messages.is_empty() {
        if let Some(f) = fetched {
            messages = f;
        }
    }

    let mut threads_by_root: HashMap<String, Vec<MessageItem>> = HashMap::new();
    if include_threads {
        if let ConversationTarget::Channel {
            ref team_id,
            ref channel_id,
        } = conv.target
        {
            let root_ids: Vec<&str> = messages
                .iter()
                .filter(|m| m.reply_count > 0)
                .map(|m| m.msg_id.as_str())
                .collect();

            if offline {
                for msg_id in root_ids {
                    let replies = storage.list_thread_replies(&conv.conv_key, msg_id, limit)?;
                    if !replies.is_empty() {
                        threads_by_root.insert(msg_id.to_string(), replies);
                    }
                }
            } else {
                let fetched: Vec<(String, Vec<MessageItem>)> = stream::iter(root_ids)
                    .map(|msg_id| {
                        let graph = &graph;
                        let storage = &storage;
                        async move {
                            let r = graph
                                .list_thread_replies(team_id, channel_id, msg_id, limit)
                                .await
                                .unwrap_or_default();
                            if !r.is_empty() {
                                let _ = storage.upsert_messages(&r);
                            }
                            (msg_id.to_string(), r)
                        }
                    })
                    .buffer_unordered(6)
                    .collect()
                    .await;

                for (msg_id, replies) in fetched {
                    if !replies.is_empty() {
                        threads_by_root.insert(msg_id, replies);
                    }
                }
            }
        }
    }

    hydrate_messages_attachments(&mut messages);
    for replies in threads_by_root.values_mut() {
        hydrate_messages_attachments(replies);
    }

    let body = match format {
        "json" => {
            if include_threads {
                let entries: Vec<serde_json::Value> = messages
                    .iter()
                    .map(|m| {
                        let replies = threads_by_root
                            .get(&m.msg_id)
                            .cloned()
                            .unwrap_or_default();
                        json!({
                            "message": m,
                            "replies": replies,
                        })
                    })
                    .collect();
                serde_json::to_string_pretty(&json!({
                    "conversation": conv,
                    "items": entries,
                }))?
            } else {
                serde_json::to_string_pretty(&json!({
                    "conversation": conv,
                    "messages": messages,
                }))?
            }
        }
        "txt" => {
            let mut out_str = format!("# {} ({})\n\n", conv.title, conv.conv_key);
            for m in &messages {
                let plain = if !m.body_plain.trim().is_empty() {
                    m.body_plain.trim().to_string()
                } else {
                    parser::html_and_cards_to_plain(&m.body_html, &m.attachments)
                };
                out_str.push_str(&format!("[{}] {}: {}\n", m.created_at, m.author_name, plain));
                if let Some(replies) = threads_by_root.get(&m.msg_id) {
                    for r in replies {
                        let rplain = if !r.body_plain.trim().is_empty() {
                            r.body_plain.trim().to_string()
                        } else {
                            parser::html_and_cards_to_plain(&r.body_html, &r.attachments)
                        };
                        out_str.push_str(&format!(
                            "  ↳ [{}] {}: {}\n",
                            r.created_at, r.author_name, rplain
                        ));
                    }
                }
            }
            out_str
        }
        _ => {
            let mut out_str = format!("# {}\n\n", conv.title);
            for m in &messages {
                if let Some(ref subj) = m.subject {
                    if !subj.trim().is_empty() {
                        out_str.push_str(&format!("## {}\n\n", subj.trim()));
                    }
                }
                let md = parser::html_and_cards_to_markdown(&m.body_html, &m.attachments);
                let text = if md.trim().is_empty() {
                    m.body_plain.trim().to_string()
                } else {
                    md.trim().to_string()
                };
                out_str.push_str(&format!(
                    "- **{}** `{}`\n\n  {}\n\n",
                    m.author_name,
                    m.created_at,
                    text.replace('\n', "\n  ")
                ));
                if let Some(replies) = threads_by_root.get(&m.msg_id) {
                    for r in replies {
                        let rmd = parser::html_and_cards_to_markdown(&r.body_html, &r.attachments);
                        let rtext = if rmd.trim().is_empty() {
                            r.body_plain.trim().to_string()
                        } else {
                            rmd.trim().to_string()
                        };
                        out_str.push_str(&format!(
                            "  - ↳ **{}** `{}`: {}\n",
                            r.author_name, r.created_at, rtext
                        ));
                    }
                    out_str.push('\n');
                }
            }
            out_str
        }
    };

    match out {
        Some(path) => {
            config::ensure_parent(path)?;
            std::fs::write(path, body)
                .with_context(|| format!("Failed to write export file: {}", path.display()))?;
            println!("Exported: {}", path.display());
        }
        None => print!("{body}"),
    }
    Ok(())
}

async fn cmd_presence(
    cfg: &AppConfig,
    set: Option<&str>,
    activity: Option<&str>,
    expiration_mins: Option<u32>,
    json_out: bool,
) -> Result<()> {
    let (auth, graph, storage) = open_runtime(cfg, true).await?;

    if let Some(raw_avail) = set {
        let availability = normalize_presence_availability(raw_avail)?;
        let act = activity
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .unwrap_or(&availability);
        graph
            .set_my_presence(&availability, act, expiration_mins)
            .await?;

        let latest = graph.get_my_presence().await.ok();
        let my_uid = latest
            .as_ref()
            .map(|p| p.user_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| graph.my_user_id().map(str::to_string));
        let my_uid = match my_uid {
            Some(id) => Some(id),
            None => auth.current_tokens().await.and_then(|t| t.user_id),
        };
        if let Some(uid) = my_uid.filter(|id| !id.is_empty()) {
            let status = latest.clone().unwrap_or(crate::models::PresenceStatus {
                user_id: uid,
                availability: availability.clone(),
                activity: act.to_string(),
            });
            let _ = storage.upsert_presences(std::slice::from_ref(&status));
        }
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "availability": availability,
                    "activity": act,
                    "expiration_mins": expiration_mins,
                    "presence": latest,
                }))?
            );
        } else {
            println!("Presence updated: {availability} ({act})");
        }
    } else {
        let status = graph.get_my_presence().await?;
        if json_out {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            println!("Current presence: {} ({})", status.availability, status.activity);
        }
    }
    Ok(())
}

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim_end_matches(&['\r', '\n'][..]).to_string())
}

// ---------------------------------------------------------------------------
// Shared `pub(crate)` helpers reused by `serve.rs` and `mcp.rs`
// ---------------------------------------------------------------------------

pub(crate) fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, 1_000)
}

pub(crate) struct ReadMessagesResult {
    pub messages: Vec<MessageItem>,
    pub has_more: bool,
    pub next_before: Option<String>,
}

pub(crate) enum MessageEdit<'a> {
    Markdown(&'a str),
    Text { find: &'a str, replace: &'a str },
    Subject(&'a str),
}

impl<'a> MessageEdit<'a> {
    pub(crate) fn from_args(
        markdown: Option<&'a str>,
        find: Option<&'a str>,
        replace: Option<&'a str>,
        subject: Option<&'a str>,
    ) -> Result<Self> {
        if let Some(subject) = subject {
            if markdown.is_some() || find.is_some() || replace.is_some() {
                bail!("Subject edits must be requested on their own.");
            }
            let subject = subject.trim();
            if subject.is_empty() || subject.contains('\0') {
                bail!("Subject must be non-empty text without NUL characters.");
            }
            return Ok(Self::Subject(subject));
        }
        match (markdown, find, replace) {
            (Some(markdown), None, None) if !markdown.trim().is_empty() => {
                Ok(Self::Markdown(markdown))
            }
            (Some(_), None, None) => bail!("Edited message body is empty."),
            (None, Some(find), Some(replace)) if !find.is_empty() => {
                if find.contains('\0') || replace.contains('\0') {
                    bail!("Find and replacement text must be free of NUL characters.");
                }
                Ok(Self::Text { find, replace })
            }
            (None, Some(_), Some(_)) => bail!("Find text is empty."),
            _ => bail!("Choose a full Markdown body, both find and replace text, or a subject."),
        }
    }
}

pub(crate) async fn edit_sent_message(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    msg_id: &str,
    parent_msg_id: Option<&str>,
    edit: MessageEdit<'_>,
) -> Result<()> {
    let parent_msg_id = parent_msg_id.filter(|id| !id.trim().is_empty());
    let saved = match edit {
        MessageEdit::Markdown(markdown) => {
            let compiled =
                parser::compile_outgoing_markdown(markdown, None, vec![], vec![], vec![], None);
            graph
                .edit_message(&conv.target, msg_id, parent_msg_id, &compiled.body_html)
                .await?;
            let _ = storage.apply_local_edit(
                &conv.conv_key,
                msg_id,
                &compiled.body_html,
                &Utc::now().to_rfc3339(),
            );
            return Ok(());
        }
        MessageEdit::Text { find, replace } => graph
            .replace_message_text(&conv.target, msg_id, parent_msg_id, find, replace)
            .await?,
        MessageEdit::Subject(subject) => {
            if parent_msg_id.is_some() {
                bail!("Subject edits require a channel root post.");
            }
            graph.edit_channel_post_subject(&conv.target, msg_id, subject).await?
        }
    };
    storage.upsert_messages(&[saved]).context(
        "Graph returned the edited message, but updating the local cache failed",
    )?;
    Ok(())
}

pub(crate) async fn update_group_chat_topic(
    storage: &Storage,
    graph: &GraphClient,
    target: &str,
    topic: &str,
) -> Result<ConversationSummary> {
    let topic = topic.trim();
    if topic.is_empty() || topic.contains([':', '\0']) || topic.chars().count() > 250 {
        bail!("Group chat topic must contain 1 to 250 characters and cannot contain colons or NUL characters.");
    }
    let conv = resolve_target(storage, graph, target, false).await?;
    let ConversationTarget::Chat { chat_id } = conv.target else {
        bail!("Chat topic changes require an existing group chat.");
    };
    let updated = graph.update_group_chat_topic(&chat_id, topic).await?;
    storage.save_updated_chat_topic(&updated).context(
        "Graph returned the updated group chat topic, but updating the local cache failed",
    )
}

fn compact_conversation(conv: &ConversationSummary) -> serde_json::Value {
    json!({ "conv_key": conv.conv_key, "title": conv.title })
}

fn compact_message(msg: &MessageItem) -> serde_json::Value {
    let mut value = json!({
        "msg_id": msg.msg_id,
        "author_id": msg.author_id,
        "author_name": msg.author_name,
        "created_at": msg.created_at,
        "body_plain": msg.body_plain,
    });
    for (key, value_if_present) in [
        ("last_modified_at", &msg.last_modified_at),
        ("parent_msg_id", &msg.parent_msg_id),
        ("reply_to_id", &msg.reply_to_id),
    ] {
        if let Some(text) = value_if_present {
            value[key] = json!(text);
        }
    }
    if !msg.attachments.is_empty() {
        value["attachments"] = serde_json::Value::Array(
            msg.attachments
                .iter()
                .map(|att| json!({ "id": att.id, "name": att.name }))
                .collect(),
        );
    }
    value
}

pub(crate) fn read_messages_json(
    conv: &ConversationSummary,
    res: &ReadMessagesResult,
    compact: bool,
) -> serde_json::Value {
    let (conversation, messages) = if compact {
        (
            compact_conversation(conv),
            serde_json::Value::Array(res.messages.iter().map(compact_message).collect()),
        )
    } else {
        (json!(conv), json!(res.messages))
    };
    json!({
        "conversation": conversation,
        "messages": messages,
        "has_more": res.has_more,
        "next_before": res.next_before,
    })
}

pub(crate) fn thread_json(
    conv: &ConversationSummary,
    thread: &ThreadView,
    compact: bool,
) -> serde_json::Value {
    if compact {
        let replies: Vec<_> = thread.replies.iter().map(compact_message).collect();
        json!({
            "conversation": compact_conversation(conv),
            "thread": { "root": compact_message(&thread.root), "replies": replies },
        })
    } else {
        json!({ "conversation": conv, "thread": thread })
    }
}

pub(crate) fn read_thread_offline(
    storage: &Storage,
    conv: &ConversationSummary,
    root_msg_id: &str,
    limit: usize,
) -> Result<Option<ThreadView>> {
    if !matches!(conv.target, ConversationTarget::Channel { .. }) {
        bail!("Thread reads require a team channel target: {}", conv.conv_key);
    }
    let mut thread = storage.get_fetched_thread(&conv.conv_key, root_msg_id, limit)?;
    if let Some(view) = thread.as_mut() {
        hydrate_thread_attachments(view);
    }
    Ok(thread)
}

pub(crate) fn thread_cache_miss(root_msg_id: &str, limit: usize) -> anyhow::Error {
    anyhow::anyhow!(
        "Thread not found in local cache at reply limit {limit}: {root_msg_id}. \
         Read the thread online to cache the requested replies."
    )
}

pub(crate) async fn read_thread(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    root_msg_id: &str,
    limit: usize,
) -> Result<ThreadView> {
    let ConversationTarget::Channel { team_id, channel_id } = &conv.target else {
        bail!("Thread reads require a team channel target: {}", conv.conv_key);
    };
    let (mut thread, complete) = graph
        .get_thread_with_coverage(team_id, channel_id, root_msg_id, limit)
        .await?;
    storage.save_fetched_thread(&thread, complete)?;
    hydrate_thread_attachments(&mut thread);
    Ok(thread)
}

pub(crate) fn is_forbidden_error(err: &anyhow::Error) -> bool {
    let lower = format!("{err:#}").to_ascii_lowercase();
    lower.contains("403")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient privileges")
        || lower.contains("missing scope")
        || lower.contains("aclcheckfailed")
        || lower.contains("insufficient graph permissions")
        || lower.contains("insufficient permissions")
}

pub(crate) async fn wrap_graph_permission_error(
    graph: &GraphClient,
    err: anyhow::Error,
) -> anyhow::Error {
    let raw = format!("{err:#}");
    let token = graph.auth().current_tokens().await.map(|t| t.access_token);
    if let Some(explained) = explain_graph_permission_error(&raw, token.as_deref()) {
        anyhow::anyhow!("{explained}\nRaw error: {raw}")
    } else {
        err
    }
}

/// Resolves `--before` (`TIMESTAMP_OR_MSG_ID`) into `(before_created_at, Option<before_msg_id>)`.
pub(crate) fn resolve_before_cursor(
    storage: &Storage,
    conv_key: &str,
    before_input: &str,
) -> Result<(String, Option<String>)> {
    let trimmed = before_input.trim();
    if trimmed.is_empty() {
        bail!("--before value is empty.");
    }

    if let Some(msg) = storage.get_message(conv_key, trimmed)? {
        return Ok((msg.created_at, Some(msg.msg_id)));
    }

    let ts = normalize_before_timestamp(trimmed)?;
    Ok((ts, None))
}

fn normalize_before_timestamp(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true));
    }
    if NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").is_ok() {
        return Ok(format!("{trimmed}T00:00:00Z"));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d_%H:%M:%S",
        "%Y-%m-%d_%H:%M",
    ] {
        if let Ok(ndt) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            let local_dt = Local
                .from_local_datetime(&ndt)
                .single()
                .or_else(|| Local.from_local_datetime(&ndt).earliest())
                .context("Failed to convert local timestamp")?;
            return Ok(local_dt
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true));
        }
    }
    bail!(
        "Invalid --before value: '{trimmed}' (expected ISO-8601 timestamp, YYYY-MM-DD, or cached message ID)"
    )
}

fn trim_older_window(mut rows: Vec<MessageItem>, limit: usize) -> (Vec<MessageItem>, bool) {
    if rows.len() > limit {
        let excess = rows.len() - limit;
        rows.drain(..excess);
        (rows, true)
    } else {
        (rows, false)
    }
}

pub(crate) fn hydrate_message_attachments(msg: &mut MessageItem) {
    if let std::borrow::Cow::Owned(effective) = msg.effective_attachments() {
        msg.attachments = effective;
    }
}

pub(crate) fn hydrate_messages_attachments(messages: &mut [MessageItem]) {
    for msg in messages {
        hydrate_message_attachments(msg);
    }
}

pub(crate) fn hydrate_thread_attachments(thread: &mut crate::models::ThreadView) {
    hydrate_message_attachments(&mut thread.root);
    hydrate_messages_attachments(&mut thread.replies);
}

pub(crate) fn read_conversation_messages_offline(
    storage: &Storage,
    conv: &ConversationSummary,
    limit: usize,
    before: Option<&str>,
) -> Result<ReadMessagesResult> {
    let limit = clamp_limit(limit);
    let (mut messages, has_more) = match before {
        None => {
            let rows = storage.list_root_messages(&conv.conv_key, limit + 1, None)?;
            trim_older_window(rows, limit)
        }
        Some(raw_before) => {
            let (before_ts, before_id) = resolve_before_cursor(storage, &conv.conv_key, raw_before)?;
            let rows = storage.list_older_root_messages(
                &conv.conv_key,
                limit + 1,
                &before_ts,
                before_id.as_deref(),
            )?;
            trim_older_window(rows, limit)
        }
    };
    hydrate_messages_attachments(&mut messages);
    let next_before = messages.first().map(|m| m.msg_id.clone());
    Ok(ReadMessagesResult {
        messages,
        has_more,
        next_before,
    })
}

pub(crate) async fn read_conversation_messages(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    limit: usize,
    before: Option<&str>,
    offline: bool,
    fetch_older: bool,
) -> Result<ReadMessagesResult> {
    let limit = clamp_limit(limit);
    if offline {
        return read_conversation_messages_offline(storage, conv, limit, before);
    }

    let scope_key = SyncCursor::history_scope(&conv.conv_key);
    match before {
        None => {
            let (fetched, next_link) = match graph.list_messages_with_cursor(&conv.target, limit).await
            {
                Ok(res) => res,
                Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
            };
            storage.upsert_messages(&fetched)?;
            if storage.get_sync_cursor(&scope_key)?.is_none() || limit > 50 {
                let oldest_ts = if next_link.is_none() {
                    Some(SyncCursor::HISTORY_EOF.to_string())
                } else {
                    fetched
                        .iter()
                        .filter(|m| m.parent_msg_id.is_none())
                        .map(|m| m.created_at.clone())
                        .min()
                };
                let _ = storage.save_sync_cursor(&SyncCursor {
                    scope_key: scope_key.clone(),
                    delta_link: next_link.clone(),
                    oldest_msg_ts: oldest_ts,
                    synced_at: Utc::now().to_rfc3339(),
                });
            }

            let rows = storage.list_root_messages(&conv.conv_key, limit + 1, None)?;
            let (mut messages, mut has_more) = trim_older_window(rows, limit);
            if messages.is_empty() {
                messages = fetched;
            }
            if !has_more {
                has_more = next_link.is_some()
                    || storage
                        .get_sync_cursor(&scope_key)
                        .ok()
                        .flatten()
                        .is_some_and(|c| !c.is_history_eof() && c.delta_link.is_some());
            }
            hydrate_messages_attachments(&mut messages);
            let next_before = messages.first().map(|m| m.msg_id.clone());
            Ok(ReadMessagesResult {
                messages,
                has_more,
                next_before,
            })
        }
        Some(raw_before) => {
            let (before_ts, before_id) = resolve_before_cursor(storage, &conv.conv_key, raw_before)?;
            if fetch_older {
                let cursor = storage.get_sync_cursor(&scope_key)?;
                let cursor_is_eof = cursor.as_ref().is_some_and(SyncCursor::is_history_eof);
                let mut local = storage.list_older_root_messages(
                    &conv.conv_key,
                    limit,
                    &before_ts,
                    before_id.as_deref(),
                )?;
                if local.len() >= limit || cursor_is_eof {
                    // Online history revalidation: reconcile the requested older window against Graph
                    // so edits/deletes/reactions in previously cached older pages are reflected.
                    let (fetched, new_next_link) = match graph
                        .fetch_older_messages(&conv.target, None, Some(&before_ts), limit)
                        .await
                    {
                        Ok(res) => res,
                        Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
                    };
                    if !fetched.is_empty() {
                        storage.upsert_messages(&fetched)?;
                    }
                    if new_next_link.is_none() || !cursor_is_eof {
                        let oldest_ts = if new_next_link.is_none() {
                            Some(SyncCursor::HISTORY_EOF.to_string())
                        } else {
                            fetched
                                .iter()
                                .filter(|m| m.parent_msg_id.is_none())
                                .map(|m| m.created_at.clone())
                                .min()
                                .or_else(|| cursor.as_ref().and_then(|c| c.oldest_msg_ts.clone()))
                        };
                        let _ = storage.save_sync_cursor(&SyncCursor {
                            scope_key: scope_key.clone(),
                            delta_link: new_next_link,
                            oldest_msg_ts: oldest_ts,
                            synced_at: Utc::now().to_rfc3339(),
                        });
                    }
                } else {
                    while local.len() < limit {
                        let cur = storage.get_sync_cursor(&scope_key)?;
                        if cur.as_ref().is_some_and(SyncCursor::is_history_eof) {
                            break;
                        }
                        let saved_link = cur.as_ref().and_then(|c| c.delta_link.clone());
                        let (fetched, new_next_link) = match graph
                            .fetch_older_messages(
                                &conv.target,
                                saved_link.as_deref(),
                                Some(&before_ts),
                                limit - local.len(),
                            )
                            .await
                        {
                            Ok(res) => res,
                            Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
                        };

                        if !fetched.is_empty() {
                            storage.upsert_messages(&fetched)?;
                        }
                        let oldest_ts = if new_next_link.is_none() {
                            Some(SyncCursor::HISTORY_EOF.to_string())
                        } else {
                            fetched
                                .first()
                                .map(|m| m.created_at.clone())
                                .or_else(|| cur.as_ref().and_then(|c| c.oldest_msg_ts.clone()))
                        };
                        let _ = storage.save_sync_cursor(&SyncCursor {
                            scope_key: scope_key.clone(),
                            delta_link: new_next_link.clone(),
                            oldest_msg_ts: oldest_ts,
                            synced_at: Utc::now().to_rfc3339(),
                        });

                        local = storage.list_older_root_messages(
                            &conv.conv_key,
                            limit,
                            &before_ts,
                            before_id.as_deref(),
                        )?;
                        if new_next_link.is_none()
                            || (fetched.is_empty() && new_next_link == saved_link)
                        {
                            break;
                        }
                    }
                }
            }

            let rows = storage.list_older_root_messages(
                &conv.conv_key,
                limit + 1,
                &before_ts,
                before_id.as_deref(),
            )?;
            let (mut messages, mut has_more) = trim_older_window(rows, limit);
            if !has_more {
                has_more = storage
                    .get_sync_cursor(&scope_key)
                    .ok()
                    .flatten()
                    .is_some_and(|c| !c.is_history_eof() && c.delta_link.is_some());
            }
            hydrate_messages_attachments(&mut messages);
            let next_before = messages.first().map(|m| m.msg_id.clone());
            Ok(ReadMessagesResult {
                messages,
                has_more,
                next_before,
            })
        }
    }
}

pub(crate) async fn resolve_target(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
    offline: bool,
) -> Result<ConversationSummary> {
    if offline {
        let cached = storage.list_conversations()?;
        resolver::resolve_from_slice(&cached, query)
    } else {
        resolver::resolve_conversation(storage, graph, query).await
    }
}

pub(crate) fn parse_mentions_cli(values: &[String]) -> Result<Vec<MentionInfo>> {
    values
        .iter()
        .enumerate()
        .map(|(idx, raw)| parse_mention_spec(raw, idx as i32))
        .collect()
}

pub(crate) fn parse_mention_spec(spec: &str, id: i32) -> Result<MentionInfo> {
    let Some((uid, name)) = spec.split_once(':') else {
        bail!("Invalid mention format: '{spec}' (expected user_id:DisplayName)");
    };
    let user_id = uid.trim();
    let display_name = name.trim();
    if user_id.is_empty() || display_name.is_empty() {
        bail!("Mention user_id and DisplayName cannot be empty: '{spec}'");
    }
    Ok(MentionInfo {
        id,
        user_id: user_id.to_string(),
        display_name: display_name.to_string(),
    })
}

pub(crate) fn normalize_reaction(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Reaction name is empty.");
    }
    let lower = trimmed.to_ascii_lowercase();
    match lower.as_str() {
        "like" | "heart" | "laugh" | "surprised" | "sad" | "angry" => Ok(lower),
        _ if !trimmed.is_ascii() => Ok(trimmed.to_string()),
        _ => bail!(
            "Unsupported reaction: '{trimmed}' (use like, heart, laugh, surprised, sad, angry, or an emoji)"
        ),
    }
}

pub(crate) fn normalize_presence_availability(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "available" => Ok("Available".to_string()),
        "busy" => Ok("Busy".to_string()),
        "donotdisturb" | "do_not_disturb" | "dnd" => Ok("DoNotDisturb".to_string()),
        "berightback" | "be_right_back" | "brb" => Ok("BeRightBack".to_string()),
        "away" => Ok("Away".to_string()),
        "reset" => Ok("Reset".to_string()),
        other => bail!(
            "Unsupported presence availability: '{other}' (Available|Busy|DoNotDisturb|BeRightBack|Away|Reset)"
        ),
    }
}

pub(crate) fn image_mime_for_path(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

pub(crate) async fn build_quote_reply(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
    msg_id: &str,
) -> Option<QuotedReply> {
    if conv.kind == ConversationKind::Channel {
        return None;
    }
    let msg = match storage.get_message(&conv.conv_key, msg_id).ok().flatten() {
        Some(m) => Some(m),
        None => {
            if let Ok(msgs) = graph.list_messages(&conv.target, 50).await {
                let _ = storage.upsert_messages(&msgs);
                msgs.into_iter().find(|m| m.msg_id == msg_id)
            } else {
                None
            }
        }
    };
    msg.map(|m| QuotedReply {
        msg_id: m.msg_id,
        author_name: m.author_name,
        preview_plain: truncate_single_line(&m.body_plain, 120),
    })
}

pub(crate) async fn send_file_or_inline(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    path: &Path,
    caption: Option<&str>,
    thread_root_id: Option<&str>,
    inline: bool,
) -> Result<MessageItem> {
    if !path.exists() || !path.is_file() {
        bail!("File not found: {}", path.display());
    }

    let mut sent = if inline {
        let mime_type = image_mime_for_path(path).with_context(|| {
            format!(
                "--inline only supports image files (png/jpg/jpeg/gif/webp/bmp): {}",
                path.display()
            )
        })?;
        let bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read image file: {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(ToString::to_string);
        let temp_id = "1".to_string();
        let inline_upload = InlineImageUpload {
            temp_id: temp_id.clone(),
            mime_type: mime_type.to_string(),
            bytes,
            file_name: file_name.clone(),
        };
        let mut outgoing = parser::compile_outgoing_markdown(
            caption.unwrap_or(""),
            None,
            vec![],
            vec![],
            vec![inline_upload],
            None,
        );
        let marker = format!("hostedContents/{temp_id}/$value");
        if !outgoing.body_html.contains(&marker) {
            let alt = file_name.as_deref().unwrap_or("image");
            outgoing.body_html.push_str(&format!(
                "<p><img src=\"../{marker}\" alt=\"{alt}\" /></p>"
            ));
        }
        if let Some(root_id) = thread_root_id {
            graph.reply_message(&conv.target, root_id, &outgoing).await?
        } else {
            graph.send_message(&conv.target, &outgoing).await?
        }
    } else {
        let caption_html = caption
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| {
                parser::compile_outgoing_markdown(c, None, vec![], vec![], vec![], None).body_html
            });
        graph
            .send_file(&conv.target, path, caption_html.as_deref(), thread_root_id)
            .await?
    };

    let _ = storage.upsert_messages(std::slice::from_ref(&sent));
    hydrate_message_attachments(&mut sent);
    Ok(sent)
}

pub(crate) async fn find_message_with_fallback(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    msg_id: &str,
    thread_root_id: Option<&str>,
) -> Result<MessageItem> {
    if let Some(msg) = storage.get_message(&conv.conv_key, msg_id)? {
        return Ok(msg);
    }

    if let (
        Some(root_id),
        ConversationTarget::Channel {
            team_id,
            channel_id,
        },
    ) = (thread_root_id, &conv.target)
    {
        let thread = graph
            .get_thread(team_id, channel_id, root_id, 50)
            .await?;
        let mut all = Vec::with_capacity(1 + thread.replies.len());
        all.push(thread.root);
        all.extend(thread.replies);
        let _ = storage.upsert_messages(&all);
        if let Some(found) = all.into_iter().find(|m| m.msg_id == msg_id) {
            return Ok(found);
        }
    }

    let msgs = graph.list_messages(&conv.target, 50).await?;
    let _ = storage.upsert_messages(&msgs);
    msgs.into_iter()
        .find(|m| m.msg_id == msg_id)
        .with_context(|| format!("Message not found: {msg_id}"))
}

pub(crate) async fn download_message_attachment(
    graph: &GraphClient,
    storage: &Storage,
    conv: &ConversationSummary,
    msg_id: &str,
    index: usize,
    thread_root_id: Option<&str>,
    dest_dir: &Path,
) -> Result<DownloadedFile> {
    let msg = find_message_with_fallback(graph, storage, conv, msg_id, thread_root_id).await?;
    let attachments = msg.effective_attachments();
    if attachments.is_empty() {
        bail!("Message has no attachments: {msg_id}");
    }
    let att = attachments.get(index).with_context(|| {
        format!(
            "Attachment index {index} is out of range (total {})",
            attachments.len()
        )
    })?;
    graph.download_attachment(att, dest_dir).await
}

// ---------------------------------------------------------------------------
// Update & Terminal Image Preview commands
// ---------------------------------------------------------------------------

async fn cmd_update(
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

pub(crate) fn default_preview_dimensions(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    let (term_cols, term_rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let c = cols.unwrap_or_else(|| term_cols.clamp(20, 120));
    let r = rows.unwrap_or_else(|| term_rows.saturating_sub(4).clamp(10, 40));
    (c, r)
}

fn resolve_image_protocol_preference(
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

fn print_rendered_preview(rendered: &str) -> Result<()> {
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}

fn cmd_preview_local_file(
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
async fn cmd_preview_teams(
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


fn format_unix_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let single = input.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_tui_without_subcommand() {
        let cli = Cli::try_parse_from(["teams"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn parses_aliases_and_flags() {
        let cli = Cli::try_parse_from(["teams", "ls", "--limit", "200", "--refresh", "--json"])
            .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Chats {
                limit: 200,
                refresh: true,
                cached: false,
                json: true
            }
        ));

        // Default `teams read` limit is 30, supports `--before` and `--thread`
        let cli = Cli::try_parse_from(["teams", "read", "Engineering/General"]).unwrap();
        match cli.command.unwrap() {
            Command::Read {
                target,
                limit,
                before,
                thread,
                offline,
                cached,
                json,
                compact,
            } => {
                assert_eq!(target, "Engineering/General");
                assert_eq!(limit, 30);
                assert!(before.is_none());
                assert!(thread.is_none());
                assert!(!offline);
                assert!(!cached);
                assert!(!json);
                assert!(!compact);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "teams",
            "read",
            "chat:1",
            "--before",
            "2026-11-01",
            "--limit",
            "100",
            "--thread",
            "root-1",
            "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Read {
                target,
                limit,
                before,
                thread,
                ..
            } => {
                assert_eq!(target, "chat:1");
                assert_eq!(limit, 100);
                assert_eq!(before.as_deref(), Some("2026-11-01"));
                assert_eq!(thread.as_deref(), Some("root-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        // `teams sync --chats --depth --target`
        let cli = Cli::try_parse_from(["teams", "sync"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Sync {
                target: None,
                chats: 100,
                depth: 50,
                json: false
            }
        ));
        let cli = Cli::try_parse_from([
            "teams", "sync", "--target", "Engineering/General", "--chats", "250", "--depth", "500", "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Sync {
                target,
                chats,
                depth,
                json,
            } => {
                assert_eq!(target.as_deref(), Some("Engineering/General"));
                assert_eq!(chats, 250);
                assert_eq!(depth, 500);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "teams",
            "msg",
            "Engineering/General",
            "Hello",
            "--subject",
            "Notice",
            "--mention",
            "u1:Alice",
            "--important",
            "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Send {
                target,
                markdown,
                subject,
                mention,
                important,
                json,
            } => {
                assert_eq!(target, "Engineering/General");
                assert_eq!(markdown, "Hello");
                assert_eq!(subject.as_deref(), Some("Notice"));
                assert_eq!(mention, vec!["u1:Alice"]);
                assert!(important);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "teams",
            "file",
            "Alice",
            "C:\\tmp\\shot.png",
            "--caption",
            "Screenshot",
            "--thread",
            "root1",
            "--inline",
            "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::SendFile {
                target,
                path,
                caption,
                thread,
                inline,
                json,
            } => {
                assert_eq!(target, "Alice");
                assert_eq!(path, PathBuf::from("C:\\tmp\\shot.png"));
                assert_eq!(caption.as_deref(), Some("Screenshot"));
                assert_eq!(thread.as_deref(), Some("root1"));
                assert!(inline);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn parses_compact_reads_and_exclusive_edit_modes() {
        let edit = Cli::try_parse_from([
            "teams", "edit", "chan:t:c", "m1", "--find", "text", "--replace=",
        ])
        .unwrap();
        assert!(matches!(
            edit.command,
            Some(Command::Edit { new_markdown: None, find: Some(_), replace: Some(ref value), .. })
                if value.is_empty()
        ));
        assert!(Cli::try_parse_from(["teams", "edit", "chat:c", "m1", "Markdown"]).is_ok());
        assert!(Cli::try_parse_from([
            "teams", "edit", "chan:t:c", "m1", "--subject", "New title",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "teams", "edit", "chan:t:c", "m1", "--thread", "root", "--subject", "New title",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "teams", "update-chat", "chat:c", "--topic", "New topic", "--json",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "teams", "edit", "chat:c", "m1", "Markdown", "--find", "old", "--replace", "new",
        ])
        .is_err());
        assert!(
            Cli::try_parse_from(["teams", "edit", "chat:c", "m1", "--find", "old"]).is_err()
        );
        assert!(Cli::try_parse_from(["teams", "read", "chat:c", "--compact"]).is_err());
        assert!(Cli::try_parse_from([
            "teams", "thread", "chan:t:c", "m1", "--json", "--compact",
        ])
        .is_ok());
    }

    #[test]
    fn parses_serve_and_mcp_aliases() {
        let cli = Cli::try_parse_from([
            "teams",
            "api",
            "--bind",
            "0.0.0.0",
            "--port",
            "9000",
            "--token",
            "sec",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Serve { bind, port, token } => {
                assert_eq!(bind, "0.0.0.0");
                assert_eq!(port, 9000);
                assert_eq!(token.as_deref(), Some("sec"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from(["teams", "mcp-server"]).unwrap();
        assert!(matches!(cli.command.unwrap(), Command::Mcp));
    }

    #[test]
    fn parses_update_and_preview_commands() {
        let cli = Cli::try_parse_from([
            "teams",
            "update",
            "--check",
            "--force",
            "--repo",
            "org/repo",
            "--json",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Update {
                check,
                force,
                repo,
                json,
            } => {
                assert!(check);
                assert!(force);
                assert_eq!(repo.as_deref(), Some("org/repo"));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let cli = Cli::try_parse_from(["teams", "preview-file", "shot.png", "--cols", "80"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::PreviewFile {
                cols: Some(80),
                protocol: None,
                ..
            }
        ));

        let cli = Cli::try_parse_from([
            "teams",
            "preview",
            "chat:1",
            "msg-1",
            "--protocol",
            "sixel",
            "--cols",
            "100",
            "--rows",
            "30",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Preview {
                target,
                msg_id,
                cols,
                rows,
                protocol,
                ..
            } => {
                assert_eq!(target.as_deref(), Some("chat:1"));
                assert_eq!(msg_id.as_deref(), Some("msg-1"));
                assert_eq!(cols, Some(100));
                assert_eq!(rows, Some(30));
                assert_eq!(protocol.as_deref(), Some("sixel"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "teams",
            "preview-file",
            "shot.png",
            "--protocol",
            "iterm2",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::PreviewFile { path, protocol, .. } => {
                assert_eq!(path, Path::new("shot.png"));
                assert_eq!(protocol.as_deref(), Some("iterm2"));
            }
            other => panic!("unexpected: {other:?}"),
        }

        let mut cfg = AppConfig::default();
        cfg.image_protocol = config::ImageProtocolPreference::Kitty;
        assert_eq!(
            resolve_image_protocol_preference(&cfg, None).unwrap(),
            config::ImageProtocolPreference::Kitty
        );
        assert_eq!(
            resolve_image_protocol_preference(&cfg, Some("sixel")).unwrap(),
            config::ImageProtocolPreference::Sixel
        );
        assert_eq!(
            resolve_image_protocol_preference(&cfg, Some("iterm2")).unwrap(),
            config::ImageProtocolPreference::Iterm2
        );
        let invalid_err = resolve_image_protocol_preference(&cfg, Some("invalid-proto"))
            .unwrap_err()
            .to_string();
        assert!(
            invalid_err.contains("auto, kitty, iterm2, sixel, halfblock"),
            "unexpected error message: {invalid_err}"
        );
    }

    #[test]
    fn parses_mentions_and_rejects_malformed() {
        let parsed =
            parse_mentions_cli(&["user-1:Alice".to_string(), " user-2 : Bob Kim ".to_string()])
                .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].id, 0);
        assert_eq!(parsed[0].user_id, "user-1");
        assert_eq!(parsed[0].display_name, "Alice");
        assert_eq!(parsed[1].id, 1);
        assert_eq!(parsed[1].user_id, "user-2");
        assert_eq!(parsed[1].display_name, "Bob Kim");

        for bad in ["nocolon", ":noname", "nouid:", "  :  "] {
            assert!(parse_mention_spec(bad, 0).is_err(), "{bad}");
        }
    }

    #[test]
    fn normalizes_reactions_and_presence() {
        assert_eq!(normalize_reaction("LIKE").unwrap(), "like");
        assert_eq!(normalize_reaction("Heart").unwrap(), "heart");
        assert_eq!(normalize_reaction("👍").unwrap(), "👍");
        assert!(normalize_reaction("  ").is_err());
        assert!(normalize_reaction("bogus").is_err());

        assert_eq!(
            normalize_presence_availability("donotdisturb").unwrap(),
            "DoNotDisturb"
        );
        assert_eq!(normalize_presence_availability("brb").unwrap(), "BeRightBack");
        assert_eq!(normalize_presence_availability("reset").unwrap(), "Reset");
        assert!(normalize_presence_availability("unknown").is_err());
    }

    #[test]
    fn delete_gate_requires_yes_or_confirmation() {
        delete_gate(true, false, None).unwrap();
        delete_gate(true, true, None).unwrap();
        assert!(delete_gate(false, true, Some("yes")).is_err());
        delete_gate(false, false, Some("YES")).unwrap();
        assert!(delete_gate(false, false, Some("no")).is_err());
        assert!(delete_gate(false, false, None).is_err());
    }

    #[test]
    fn resolves_before_cursor_and_offline_pagination() {
        let storage = Storage::open_in_memory().unwrap();
        let conv = ConversationSummary {
            conv_key: "chat:19:hist".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "19:hist".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "History Test".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: None,
            pinned: false,
            peer_user_id: None,
            web_url: None,
        };
        storage.upsert_conversations(std::slice::from_ref(&conv)).unwrap();

        let mk_msg = |id: &str, ts: &str| MessageItem {
            msg_key: format!("chat:19:hist|{id}"),
            conv_key: "chat:19:hist".to_string(),
            msg_id: id.to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("u1".to_string()),
            author_name: "Bob".to_string(),
            is_me: false,
            created_at: ts.to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: format!("<p>{id}</p>"),
            body_plain: id.to_string(),
            attachments: vec![],
            mentions: vec![],
            reactions: vec![],
            reply_count: 0,
            web_url: None,
        };

        storage
            .upsert_messages(&[
                mk_msg("m1", "2026-10-30T10:00:00Z"),
                mk_msg("m2", "2026-10-31T12:00:00Z"),
                mk_msg("m3", "2026-11-01T09:00:00Z"),
                mk_msg("m4", "2026-11-02T09:00:00Z"),
            ])
            .unwrap();

        // Date-only `YYYY-MM-DD` normalizes to `YYYY-MM-DDT00:00:00Z`
        let (ts, id) = resolve_before_cursor(&storage, &conv.conv_key, "2026-11-01").unwrap();
        assert_eq!(ts, "2026-11-01T00:00:00Z");
        assert!(id.is_none());

        // Cached msg_id resolves to its timestamp and msg_id
        let (ts, id) = resolve_before_cursor(&storage, &conv.conv_key, "m3").unwrap();
        assert_eq!(ts, "2026-11-01T09:00:00Z");
        assert_eq!(id.as_deref(), Some("m3"));

        // Invalid input returns an error
        assert!(resolve_before_cursor(&storage, &conv.conv_key, "not-a-timestamp").is_err());

        // Offline pagination before `2026-11-01` yields m1, m2 in chronological order
        let res = read_conversation_messages_offline(&storage, &conv, 1, Some("2026-11-01"))
            .unwrap();
        assert_eq!(res.messages.len(), 1);
        assert_eq!(res.messages[0].msg_id, "m2");
        assert!(res.has_more);
        assert_eq!(res.next_before.as_deref(), Some("m2"));

        let res2 = read_conversation_messages_offline(&storage, &conv, 10, Some("m2")).unwrap();
        assert_eq!(res2.messages.len(), 1);
        assert_eq!(res2.messages[0].msg_id, "m1");
        assert!(!res2.has_more);
    }

    #[test]
    fn pure_teams_top_aliases_and_env_help() {
        assert!(matches!(
            Cli::try_parse_from(["teams", "chats", "--top", "7"]).unwrap().command.unwrap(),
            Command::Chats { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "read", "c1", "--top", "7"]).unwrap().command.unwrap(),
            Command::Read { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "thread", "c1", "m1", "--top", "7"]).unwrap().command.unwrap(),
            Command::Thread { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "search", "q", "--top", "7"]).unwrap().command.unwrap(),
            Command::Search { limit: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "sync", "--top", "7"]).unwrap().command.unwrap(),
            Command::Sync { depth: 7, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "sync", "--limit", "77"]).unwrap().command.unwrap(),
            Command::Sync { depth: 77, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "export", "c1", "--top", "7"]).unwrap().command.unwrap(),
            Command::Export { limit: 7, .. }
        ));

        let required_envs = [
            "TEAMS_TOKEN_FILE",
            "TEAMS_TOKEN_REFRESH_CMD",
            "TEAMS_ACCESS_TOKEN",
            "TEAMS_DISABLE_KEYRING",
            "TEAMS_IMAGE_PROTOCOL",
        ];
        let teams_help = <Cli as clap::CommandFactory>::command().render_help().to_string();
        for env_name in required_envs {
            assert!(teams_help.contains(env_name), "teams help missing {env_name}");
        }
        assert!(!teams_help.contains("TEAMS_PERSONA"));
        assert!(Cli::try_parse_from(["teams", "mail"]).is_err());
        assert!(Cli::try_parse_from(["teams", "notes"]).is_err());
    }

    #[test]
    fn parses_cached_and_fast_flags() {
        assert!(matches!(
            Cli::try_parse_from(["teams", "chats", "--cached"]).unwrap().command.unwrap(),
            Command::Chats { cached: true, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "chats", "--fast"]).unwrap().command.unwrap(),
            Command::Chats { cached: true, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "teams", "--fast"]).unwrap().command.unwrap(),
            Command::Teams { cached: true, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "channels", "dev", "--cached"]).unwrap().command.unwrap(),
            Command::Channels { cached: true, .. }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "read", "c1", "--fast"]).unwrap().command.unwrap(),
            Command::Read { cached: true, .. }
        ));
    }

    #[test]
    fn fast_help_version_and_cache_cursor_helpers_work() {
        assert!(try_fast_help_or_version_from(["teams", "--help"], Persona::Teams).is_some());
        assert!(try_fast_help_or_version_from(["teams", "-V"], Persona::Teams).is_some());
        assert!(try_fast_help_or_version_from(["teams", "read", "--help"], Persona::Teams).is_some());
        assert!(try_fast_help_or_version_from(["teams", "chats"], Persona::Teams).is_none());
        assert!(try_fast_help_or_version_from(["teams", "send", "a", "--", "--help"], Persona::Teams).is_none());

        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"oid":"user-123","sub":"sub-1"}"#);
        let jwt = format!("Bearer header.{payload}.sig");
        assert_eq!(extract_user_id_from_jwt(&jwt).as_deref(), Some("user-123"));

        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let cred_path = tmp.path().join("credentials.json");
        std::fs::create_dir_all(data_dir.join("solo-user")).unwrap();
        std::fs::write(data_dir.join("solo-user").join("cache.db"), b"").unwrap();

        let cfg = AppConfig::default();
        assert_eq!(
            resolve_storage_db_path_fast(&cfg, None, &data_dir, &cred_path).unwrap(),
            data_dir.join("solo-user").join("cache.db")
        );

        std::fs::create_dir_all(data_dir.join("second-user")).unwrap();
        std::fs::write(data_dir.join("second-user").join("cache.db"), b"").unwrap();
        assert!(resolve_storage_db_path_fast(&cfg, None, &data_dir, &cred_path).is_none());

        std::fs::write(&cred_path, r#"{"user_id":"second-user"}"#).unwrap();
        assert_eq!(
            resolve_storage_db_path_fast(&cfg, None, &data_dir, &cred_path).unwrap(),
            data_dir.join("second-user").join("cache.db")
        );

        assert_eq!(
            resolve_storage_db_path_fast(&cfg, Some(&jwt), &data_dir, &cred_path).unwrap(),
            data_dir.join("user-123").join("cache.db")
        );

        assert_eq!(parse_cli_cache_secs(None), 0);
        assert_eq!(parse_cli_cache_secs(Some(" 60 ")), 60);
        assert_eq!(parse_cli_cache_secs(Some("bad")), 0);

        let storage = Storage::open_in_memory().unwrap();
        assert!(!is_cursor_fresh(&storage, &["cli:chats"], 60));
        record_cli_sync_cursor(&storage, "cli:chats");
        assert!(is_cursor_fresh(&storage, &["cli:chats"], 60));
        assert!(!is_cursor_fresh(&storage, &["cli:chats"], 0));
    }

    #[test]
    fn parses_daemon_subcommands() {
        assert!(matches!(
            Cli::try_parse_from(["teams", "daemon", "start"])
                .unwrap()
                .command
                .unwrap(),
            Command::Daemon {
                command: DaemonCommand::Start { json: false }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "daemon", "start", "--json"])
                .unwrap()
                .command
                .unwrap(),
            Command::Daemon {
                command: DaemonCommand::Start { json: true }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "daemon", "stop"])
                .unwrap()
                .command
                .unwrap(),
            Command::Daemon {
                command: DaemonCommand::Stop { json: false }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "daemon", "status"])
                .unwrap()
                .command
                .unwrap(),
            Command::Daemon {
                command: DaemonCommand::Status { json: false }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "daemon", "restart"])
                .unwrap()
                .command
                .unwrap(),
            Command::Daemon {
                command: DaemonCommand::Restart { json: false }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["teams", "daemon", "run"])
                .unwrap()
                .command
                .unwrap(),
            Command::Daemon {
                command: DaemonCommand::Run
            }
        ));
    }

    #[tokio::test]
    async fn cached_inline_hosted_image_exposes_effective_attachment_for_cli_helpers() {
        use axum::{http::StatusCode, routing::get, Router};

        let storage = Storage::open_in_memory().unwrap();
        let conv = ConversationSummary {
            conv_key: "chat:c1".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "c1".to_string(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Inline Hosted".to_string(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: Some("m1".to_string()),
            last_msg_author: Some("Alice".to_string()),
            last_msg_preview: Some("Screenshot".to_string()),
            last_updated_at: Some("2026-11-18T10:00:00Z".to_string()),
            pinned: false,
            peer_user_id: None,
            web_url: None,
        };
        storage.upsert_conversations(std::slice::from_ref(&conv)).unwrap();

        let graph_hosted_url =
            "https://graph.microsoft.com/v1.0/chats/c1/messages/m1/hostedContents/1/$value";
        let cached_msg = MessageItem {
            msg_key: "chat:c1|m1".to_string(),
            conv_key: "chat:c1".to_string(),
            msg_id: "m1".to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("u1".to_string()),
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: "2026-11-18T10:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: format!(r#"<p>See screenshot</p><img src="{graph_hosted_url}">"#),
            body_plain: "See screenshot".to_string(),
            attachments: vec![],
            mentions: vec![],
            reactions: vec![],
            reply_count: 0,
            web_url: None,
        };
        storage.upsert_messages(std::slice::from_ref(&cached_msg)).unwrap();

        // 1. CLI read helper hydrates attachments for --json & renders [📎 ...] tag in text.
        let read_res = read_conversation_messages_offline(&storage, &conv, 10, None).unwrap();
        assert_eq!(read_res.messages.len(), 1);
        assert_eq!(read_res.messages[0].attachments.len(), 1);
        assert_eq!(read_res.messages[0].attachments[0].id, "hosted:1");
        assert_eq!(
            read_res.messages[0].attachments[0].content_url.as_deref(),
            Some(graph_hosted_url)
        );
        let raw_msg = storage.get_message("chat:c1", "m1").unwrap().unwrap();
        assert!(raw_msg.attachments.is_empty());
        let line = format_message_line(&raw_msg, "");
        assert!(line.contains("[📎 "), "expected attachment tag in line: {line}");

        // 2. CLI preview selector finds hosted:1 even on unhydrated cached MessageItem.
        for selector in [None, Some("0"), Some("hosted:1")] {
            let att = select_teams_attachment(&raw_msg, selector).unwrap();
            assert_eq!(att.id, "hosted:1");
            assert_eq!(att.content_url.as_deref(), Some(graph_hosted_url));
            assert!(img_preview::is_image_mime_or_name(
                &att.content_type,
                att.name.as_deref()
            ));
        }

        // 3. CLI download helper fetches hosted:1 from mock Graph server for cached message.
        let png_bytes = b"\x89PNG\r\n\x1a\nfake-png-bytes".to_vec();
        let served_bytes = png_bytes.clone();
        let mock_app = Router::new().route(
            "/v1.0/chats/c1/messages/m1/hostedContents/1/$value",
            get(move || {
                let bytes = served_bytes.clone();
                async move { (StatusCode::OK, bytes) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock_server = tokio::spawn(async move {
            axum::serve(listener, mock_app).await.unwrap();
        });

        let local_hosted_url = format!(
            "http://{addr}/v1.0/chats/c1/messages/m1/hostedContents/1/$value"
        );
        let mut local_msg = cached_msg;
        local_msg.msg_key = "chat:c1|m2".to_string();
        local_msg.msg_id = "m2".to_string();
        local_msg.body_html = format!(r#"<img src="{local_hosted_url}">"#);
        storage.upsert_messages(&[local_msg]).unwrap();

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("me-1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let dl_dir = tempfile::tempdir().unwrap();

        let downloaded =
            download_message_attachment(&graph, &storage, &conv, "m2", 0, None, dl_dir.path())
                .await
                .unwrap();
        assert_eq!(downloaded.bytes_written, png_bytes.len() as u64);
        assert_eq!(std::fs::read(&downloaded.path).unwrap(), png_bytes);

        mock_server.abort();
    }
}
