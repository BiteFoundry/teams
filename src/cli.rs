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
        InlineImageUpload, MemberInfo, MentionInfo, MessageItem, QuotedReply, SyncCursor,
        TeamChannelError, TeamSummary, ThreadCoverage, ThreadView, UserProfile,
    },
    parser, resolver,
    serve::{self, ServeOptions},
    storage::Storage,
    tui, updater,
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
  TEAMS_EXTRA_CA_PEM=<PATH>             Extra trusted root CAs (PEM) for TLS-inspecting proxies; SSL_CERT_FILE and NODE_EXTRA_CA_CERTS are also read

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

impl Cli {
    pub fn parse() -> Self {
        Self::parse_from(std::env::args_os())
    }

    pub fn try_parse() -> std::result::Result<Self, clap::Error> {
        Self::try_parse_from(std::env::args_os())
    }

    pub fn parse_from<I, T>(itr: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        match Self::try_parse_from(itr) {
            Ok(cli) => cli,
            Err(err) => err.exit(),
        }
    }

    pub fn try_parse_from<I, T>(itr: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let raw: Vec<std::ffi::OsString> = itr.into_iter().map(Into::into).collect();
        match normalize_cli_args(&raw) {
            Some(normalized) => <Self as Parser>::try_parse_from(normalized),
            None => <Self as Parser>::try_parse_from(raw),
        }
    }
}

struct HyphenTextSpec {
    options: &'static [&'static str],
    positionals: &'static [usize],
}

fn canonical_hyphen_subcommand(invoked: &str) -> Option<&'static str> {
    match invoked {
        "new-chat" => Some("new-chat"),
        "update-chat" => Some("update-chat"),
        "send" | "msg" => Some("send"),
        "reply" => Some("reply"),
        "send-file" | "file" => Some("send-file"),
        "edit" => Some("edit"),
        "search" => Some("search"),
        _ => None,
    }
}

fn hyphen_text_spec(canonical_subcommand: &str) -> Option<HyphenTextSpec> {
    match canonical_subcommand {
        "new-chat" => Some(HyphenTextSpec {
            options: &["--topic", "--message"],
            positionals: &[],
        }),
        "update-chat" => Some(HyphenTextSpec {
            options: &["--topic"],
            positionals: &[],
        }),
        "send" => Some(HyphenTextSpec {
            options: &["--subject"],
            positionals: &[1],
        }),
        "reply" => Some(HyphenTextSpec {
            options: &[],
            positionals: &[2],
        }),
        "send-file" => Some(HyphenTextSpec {
            options: &["--caption"],
            positionals: &[],
        }),
        "edit" => Some(HyphenTextSpec {
            options: &["--find", "--replace", "--subject"],
            positionals: &[2],
        }),
        "search" => Some(HyphenTextSpec {
            options: &[],
            positionals: &[0],
        }),
        _ => None,
    }
}

fn starts_with_dash(tok: &std::ffi::OsStr) -> bool {
    tok.as_encoded_bytes().starts_with(b"-")
}

fn is_long_flag_token(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("--") else {
        return false;
    };
    let flag_name = rest.split_once('=').map_or(rest, |(name, _)| name);
    !flag_name.is_empty()
        && flag_name.as_bytes()[0].is_ascii_alphanumeric()
        && flag_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_flag_like_token(
    tok: &std::ffi::OsStr,
    known_short_flags: &std::collections::HashSet<char>,
) -> bool {
    let Some(s) = tok.to_str() else {
        return false;
    };
    if s == "--" {
        return true;
    }
    if is_long_flag_token(s) {
        return true;
    }
    if let Some(rest) = s.strip_prefix('-') {
        return !rest.is_empty() && rest.chars().all(|c| known_short_flags.contains(&c));
    }
    false
}

/// Fast check that avoids building/inspecting the clap `Command` tree for ordinary argv
/// with no candidate hyphen-prefixed free-text value.
fn has_candidate_hyphen_value(raw: &[std::ffi::OsString]) -> bool {
    if raw.len() < 3 {
        return false;
    }
    let Some(invoked) = raw[1].to_str() else {
        return false;
    };
    if canonical_hyphen_subcommand(invoked).is_none() {
        return false;
    }
    for tok in &raw[2..] {
        if tok.to_str() == Some("--") {
            break;
        }
        if !starts_with_dash(tok) {
            continue;
        }
        let Some(s) = tok.to_str() else {
            return true;
        };
        if !is_long_flag_token(s) && !matches!(s, "-h" | "-V") {
            return true;
        }
    }
    false
}

/// Normalizes hyphen-prefixed free-text literals (such as `"- list item"` or multiline
/// Markdown lists) for eligible subcommand arguments while preserving known/unknown flags,
/// missing option value errors, `--flag=value`, `--`, and `-h`/`--help`.
/// Returns `None` without building the clap command tree when argv needs no rewriting.
fn normalize_cli_args(raw: &[std::ffi::OsString]) -> Option<Vec<std::ffi::OsString>> {
    use std::collections::HashSet;
    use std::ffi::OsString;

    if !has_candidate_hyphen_value(raw) {
        return None;
    }
    let invoked_subcmd = raw[1].to_str()?;
    let canonical = canonical_hyphen_subcommand(invoked_subcmd)?;
    let spec = hyphen_text_spec(canonical)?;

    let mut root_cmd = <Cli as clap::CommandFactory>::command();
    root_cmd.build();
    let subcmd = root_cmd
        .get_subcommands()
        .find(|sc| sc.get_name() == canonical)?;

    let mut known_long_takes_value: HashSet<String> = HashSet::new();
    let mut known_short_flags: HashSet<char> = HashSet::from(['h', 'V']);
    for arg in subcmd.get_arguments() {
        if arg.is_positional() {
            continue;
        }
        if let Some(short) = arg.get_short() {
            known_short_flags.insert(short);
        }
        if let Some(aliases) = arg.get_all_short_aliases() {
            known_short_flags.extend(aliases);
        }
        if arg.get_action().takes_values() {
            if let Some(long) = arg.get_long() {
                known_long_takes_value.insert(format!("--{long}"));
            }
            if let Some(aliases) = arg.get_all_aliases() {
                for alias in aliases {
                    known_long_takes_value.insert(format!("--{alias}"));
                }
            }
        }
    }

    let mut in_place: Vec<OsString> = Vec::with_capacity(raw.len());
    in_place.push(raw[0].clone());
    in_place.push(raw[1].clone());

    let mut opt_tokens: Vec<OsString> = Vec::new();
    let mut pos_tokens: Vec<OsString> = Vec::new();
    let mut rewrote_any_opt = false;
    let mut needs_pos_escape = false;

    let mut i = 2;
    while i < raw.len() {
        let tok = &raw[i];
        if tok.to_str() == Some("--") {
            in_place.extend_from_slice(&raw[i..]);
            pos_tokens.extend_from_slice(&raw[i + 1..]);
            break;
        }

        if let Some(opt_name) = tok.to_str().filter(|s| known_long_takes_value.contains(*s)) {
            let allow_hyphen = spec.options.contains(&opt_name);
            match raw.get(i + 1) {
                Some(next)
                    if allow_hyphen
                        && starts_with_dash(next)
                        && !is_flag_like_token(next, &known_short_flags) =>
                {
                    let mut joined = OsString::with_capacity(opt_name.len() + 1 + next.len());
                    joined.push(opt_name);
                    joined.push("=");
                    joined.push(next);
                    in_place.push(joined.clone());
                    opt_tokens.push(joined);
                    rewrote_any_opt = true;
                    i += 2;
                    continue;
                }
                Some(next) if !starts_with_dash(next) => {
                    in_place.push(tok.clone());
                    in_place.push(next.clone());
                    opt_tokens.push(tok.clone());
                    opt_tokens.push(next.clone());
                    i += 2;
                    continue;
                }
                Some(next) if !allow_hyphen && !is_flag_like_token(next, &known_short_flags) => {
                    in_place.push(tok.clone());
                    in_place.push(next.clone());
                    opt_tokens.push(tok.clone());
                    opt_tokens.push(next.clone());
                    i += 2;
                    continue;
                }
                _ => {
                    in_place.push(tok.clone());
                    opt_tokens.push(tok.clone());
                    i += 1;
                    continue;
                }
            }
        }

        if starts_with_dash(tok) {
            if is_flag_like_token(tok, &known_short_flags) {
                in_place.push(tok.clone());
                opt_tokens.push(tok.clone());
                i += 1;
                continue;
            }
            let pos_idx = pos_tokens.len();
            if spec.positionals.contains(&pos_idx) {
                in_place.push(tok.clone());
                pos_tokens.push(tok.clone());
                if tok.to_str() != Some("-") {
                    needs_pos_escape = true;
                }
            } else {
                in_place.push(tok.clone());
                opt_tokens.push(tok.clone());
            }
            i += 1;
            continue;
        }

        in_place.push(tok.clone());
        pos_tokens.push(tok.clone());
        i += 1;
    }

    if needs_pos_escape {
        let mut out = Vec::with_capacity(2 + opt_tokens.len() + 1 + pos_tokens.len());
        out.push(raw[0].clone());
        out.push(raw[1].clone());
        out.extend(opt_tokens);
        out.push(OsString::from("--"));
        out.extend(pos_tokens);
        Some(out)
    } else if rewrote_any_opt {
        Some(in_place)
    } else {
        None
    }
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
        /// Maximum number of chats to fetch
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
        /// Maximum number of teams per page [JSON default: 50; text output lists all]
        #[arg(long, visible_alias = "top")]
        limit: Option<usize>,
        /// Read the next page from the local cache; pass next_cursor from the previous page
        #[arg(long, conflicts_with = "refresh")]
        cursor: Option<String>,
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
        /// Maximum number of channels per page [JSON default: 50; text output lists all]
        #[arg(long, visible_alias = "top")]
        limit: Option<usize>,
        /// Read the next page from the local cache; pass next_cursor from the previous page
        #[arg(long, conflicts_with = "refresh")]
        cursor: Option<String>,
        #[arg(long)]
        refresh: bool,
        /// Read from local SQLite cache first (skips network requests)
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read recent messages from a chat or channel, or a specific channel thread with `--thread`
    #[command(
        visible_alias = "cat",
        after_help = "Query modes:\n  Conversation history: teams read <TARGET> [--limit N] [--before TIMESTAMP_OR_MSG_ID]\n  Channel thread view:  teams read <TARGET> --thread <ROOT_MSG_ID> [--limit N]\nNote: --thread and --before are mutually exclusive (thread views do not support --before)."
    )]
    Read {
        target: String,
        /// Maximum number of messages, or replies when --thread includes the root separately
        #[arg(long, visible_alias = "top", default_value_t = 30)]
        limit: usize,
        /// Fetch messages older than this timestamp (ISO-8601 / YYYY-MM-DD) or message ID (conflicts with --thread)
        #[arg(long, value_name = "TIMESTAMP_OR_MSG_ID", conflicts_with = "thread")]
        before: Option<String>,
        /// Read a specific channel thread by root message ID (conflicts with --before)
        #[arg(long, conflicts_with = "before")]
        thread: Option<String>,
        #[arg(long)]
        offline: bool,
        /// Read from local SQLite cache first (falls back to network if empty or insufficient)
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
        /// Compatibility alias: compact output is the --json default
        #[arg(long, requires = "json")]
        compact: bool,
        /// Return complete conversation and message objects instead of the compact default
        #[arg(long, requires = "json", conflicts_with = "compact")]
        full: bool,
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
        /// Compatibility alias: compact output is the --json default
        #[arg(long, requires = "json")]
        compact: bool,
        /// Return complete conversation and message objects instead of the compact default
        #[arg(long, requires = "json", conflicts_with = "compact")]
        full: bool,
    },
    /// Send a Markdown message
    #[command(
        visible_alias = "msg",
        after_help = "Markdown support: converts **bold**, *italic*, `code`, ~~strike~~, [label](url), > quote, fenced code blocks, single-level `- ` bullet lists, and @mentions. Headings (#), tables (|...|), _italic_, nested lists, and numbered lists are sent verbatim."
    )]
    Send {
        target: String,
        markdown: String,
        #[arg(long)]
        subject: Option<String>,
        /// Add a mention: <user_id:DisplayName> (repeatable; @DisplayName must appear in the body or the command fails; duplicate display names are assigned in --mention order)
        #[arg(long = "mention", value_name = "user_id:DisplayName")]
        mention: Vec<String>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        json: bool,
        /// Return complete conversation and message objects instead of the compact default
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// Reply to a channel thread or quote-reply to a chat message
    Reply {
        target: String,
        msg_id: String,
        markdown: String,
        /// Add a mention: <user_id:DisplayName> (repeatable; @DisplayName must appear in the body or the command fails; duplicate display names are assigned in --mention order)
        #[arg(long = "mention", value_name = "user_id:DisplayName")]
        mention: Vec<String>,
        #[arg(long)]
        important: bool,
        #[arg(long)]
        json: bool,
        /// Return complete conversation and message objects instead of the compact default
        #[arg(long, requires = "json")]
        full: bool,
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
        /// Return complete conversation and message objects instead of the compact default
        #[arg(long, requires = "json")]
        full: bool,
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
        /// Maximum number of members per page [JSON default: 50; text output lists all]
        #[arg(long, visible_alias = "top")]
        limit: Option<usize>,
        /// Read the next page from the local cache; pass next_cursor from the previous page
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        offline: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search directory users by display name or email prefix
    Users {
        query: String,
        /// Maximum number of users to return
        #[arg(long, visible_alias = "top", default_value_t = 10)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Search messages in the local SQLite FTS5 cache
    Search {
        query: String,
        #[arg(long)]
        target: Option<String>,
        /// Maximum number of results to return
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
        /// Number of recent chats to sync
        #[arg(long, default_value_t = 100)]
        chats: usize,
        /// Number of messages per conversation/channel to backfill into local FTS5 cache
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
        /// Maximum number of messages to export
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
        /// Bearer token callers must present; without it a random one is generated and
        /// printed to stderr (loopback binds only)
        #[arg(long)]
        token: Option<String>,
        /// Run a loopback server with no bearer token at all
        #[arg(long, conflicts_with = "token")]
        allow_keyless: bool,
        /// Exact browser origin allowed to call the API, e.g. http://localhost:3000 (repeatable)
        #[arg(long = "cors-origin", value_name = "ORIGIN")]
        cors_origin: Vec<String>,
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

impl Command {
    fn json_output(&self) -> bool {
        match self {
            Command::Login { json, .. }
            | Command::Logout { json }
            | Command::Whoami { json }
            | Command::Doctor { json, .. }
            | Command::Chats { json, .. }
            | Command::NewChat { json, .. }
            | Command::UpdateChat { json, .. }
            | Command::Teams { json, .. }
            | Command::Channels { json, .. }
            | Command::Read { json, .. }
            | Command::Thread { json, .. }
            | Command::Send { json, .. }
            | Command::Reply { json, .. }
            | Command::SendFile { json, .. }
            | Command::Download { json, .. }
            | Command::React { json, .. }
            | Command::Edit { json, .. }
            | Command::Delete { json, .. }
            | Command::Members { json, .. }
            | Command::Users { json, .. }
            | Command::Search { json, .. }
            | Command::Sync { json, .. }
            | Command::Presence { json, .. }
            | Command::Update { json, .. } => *json,
            Command::Daemon { command } => match command {
                DaemonCommand::Start { json }
                | DaemonCommand::Stop { json }
                | DaemonCommand::Status { json }
                | DaemonCommand::Restart { json } => *json,
                DaemonCommand::Run => false,
            },
            Command::Export { .. }
            | Command::Preview { .. }
            | Command::PreviewFile { .. }
            | Command::Tui
            | Command::Serve { .. }
            | Command::Mcp => false,
        }
    }
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
    let json = cli.command.as_ref().is_some_and(Command::json_output);
    let mut cfg = AppConfig::load();
    match Box::pin(dispatch_command(&mut cfg, persona, cli.command)).await {
        Ok(()) => Ok(()),
        Err(err) => {
            if json {
                println!("{}", error_json(&err));
            }
            Err(err)
        }
    }
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
            limit,
            cursor,
            refresh,
            cached,
            json,
        }) => Box::pin(cmd_teams(cfg, limit, cursor.as_deref(), refresh, cached, json)).await,
        Some(Command::Channels {
            team_query,
            limit,
            cursor,
            refresh,
            cached,
            json,
        }) => {
            Box::pin(cmd_channels(
                cfg,
                &team_query,
                limit,
                cursor.as_deref(),
                refresh,
                cached,
                json,
            ))
            .await
        }
        Some(Command::Read {
            target,
            limit,
            before,
            thread,
            offline,
            cached,
            json,
            compact: _,
            full,
        }) => {
            let full_args = format_full_args(limit, before.as_deref(), offline);
            Box::pin(cmd_read(
                cfg,
                &target,
                limit,
                before.as_deref(),
                thread.as_deref(),
                offline,
                cached,
                ReadOutput {
                    json,
                    compact: !full,
                    full_args,
                },
            ))
            .await
        }
        Some(Command::Thread {
            channel_target,
            root_msg_id,
            limit,
            offline,
            json,
            compact: _,
            full,
        }) => {
            let full_args = format_full_args(limit, None, offline);
            Box::pin(cmd_thread_with_cache(
                cfg,
                &channel_target,
                &root_msg_id,
                limit,
                offline,
                false,
                ReadOutput {
                    json,
                    compact: !full,
                    full_args,
                },
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
            full,
        }) => {
            Box::pin(cmd_send(
                cfg,
                &target,
                &markdown,
                subject,
                &mention,
                important,
                json,
                full,
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
            full,
        }) => {
            Box::pin(cmd_reply(
                cfg,
                &target,
                &msg_id,
                &markdown,
                &mention,
                important,
                json,
                full,
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
            full,
        }) => {
            Box::pin(cmd_send_file(
                cfg,
                &target,
                &path,
                caption.as_deref(),
                thread.as_deref(),
                inline,
                json,
                full,
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
            limit,
            cursor,
            offline,
            json,
        }) => Box::pin(cmd_members(cfg, &target, limit, cursor.as_deref(), offline, json)).await,
        Some(Command::Users { query, limit, json }) => {
            Box::pin(cmd_users(cfg, &query, limit, json)).await
        }
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
        Some(Command::Serve {
            bind,
            port,
            token,
            allow_keyless,
            cors_origin,
        }) => {
            let (_auth, graph, storage) = Box::pin(open_runtime(cfg, false)).await?;
            Box::pin(serve::run(
                cfg.clone(),
                graph,
                storage,
                ServeOptions {
                    bind,
                    port,
                    token,
                    allow_keyless,
                    cors_origins: cors_origin,
                },
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
        let _ = crate::daemon::ensure_daemon_started(cfg, inferred_user_id.as_deref()).await;
    }
    daemon_warm
}

/// The signed-in account as far as it can be told without the OS keyring or the network.
fn current_user_id_fast(cfg: &AppConfig) -> Option<String> {
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

async fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
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
            if cached
                || daemon_warm
                || is_cursor_fresh(&storage, &["cli:chats", SyncCursor::CHATS_LIST_SCOPE], ttl)
            {
                let mut chats: Vec<ConversationSummary> = storage
                    .list_conversations()?
                    .into_iter()
                    .filter(|c| !c.kind.is_channel())
                    .collect();
                if chats_cache_satisfies_limit(&storage, chats.len(), limit) {
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
        && (cached
            || daemon_warm
            || (ttl > 0
                && is_cursor_fresh(&storage, &["cli:chats", SyncCursor::CHATS_LIST_SCOPE], ttl)));
    let mut chats =
        if !cache_usable || !chats_cache_satisfies_limit(&storage, cached_chats.len(), limit) {
            let fetched = match graph.list_chats_with_cursor(limit).await {
                Ok(items) => items,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            storage.upsert_conversations_with_null_topics(
                &fetched.chats,
                &fetched.explicit_null_topic_keys,
            )?;
            record_chats_list_cursor(&storage, fetched.next_link, fetched.eof);
            record_cli_sync_cursor(&storage, "cli:chats");
            let stored: Vec<ConversationSummary> = storage
                .list_conversations()?
                .into_iter()
                .filter(|c| !c.kind.is_channel())
                .collect();
            if stored.is_empty() {
                fetched.chats
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

fn print_teams_output(
    page: &Page<TeamSummary>,
    channel_errors: Option<&[TeamChannelError]>,
    json_out: bool,
) -> Result<()> {
    if json_out {
        let mut obj = json!({
            "teams": page.items,
            "returned": page.items.len(),
            "has_more": page.has_more,
            "next_cursor": page.next_cursor,
            "total": page.total,
        });
        if let Some(errors) = channel_errors {
            obj["channel_errors"] = json!(errors);
        }
        println!("{}", serde_json::to_string_pretty(&obj)?);
    } else if page.items.is_empty() {
        println!("No joined teams found.");
    } else {
        for team in &page.items {
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
        if let Some(ref next) = page.next_cursor {
            println!(
                "(Showing {} of {} teams. Next page: --cursor {next})",
                page.items.len(),
                page.total
            );
        }
    }
    Ok(())
}

fn print_channels_output(
    team: &TeamSummary,
    page: &Page<crate::models::ChannelSummary>,
    json_out: bool,
) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "team": { "id": team.id, "display_name": team.display_name },
                "channels": page.items,
                "returned": page.items.len(),
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
                "total": page.total,
            }))?
        );
    } else {
        println!("# {} ({})", team.display_name, team.id);
        for ch in &page.items {
            println!(
                "  {}  [{}]  {}",
                ch.conv_key(),
                ch.membership_type,
                ch.display_name
            );
        }
        if let Some(ref next) = page.next_cursor {
            println!(
                "(Showing {} of {} channels. Next page: --cursor {next})",
                page.items.len(),
                page.total
            );
        }
    }
    Ok(())
}

fn paged_list_limit(
    limit: Option<usize>,
    cursor: Option<&str>,
    json_out: bool,
    total: usize,
) -> usize {
    if json_out || limit.is_some() || cursor.is_some() {
        clamp_limit(limit.unwrap_or(50))
    } else {
        total.max(1)
    }
}

async fn cmd_teams(
    cfg: &AppConfig,
    limit: Option<usize>,
    cursor: Option<&str>,
    refresh: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    if cursor.is_some() {
        let storage = open_storage_only(cfg)?;
        let teams = storage.list_teams()?;
        let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
        let page_limit = paged_list_limit(limit, cursor, json_out, teams.len());
        let page = paginate(teams, page_limit, cursor, stamp)?;
        return print_teams_output(&page, None, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:teams"], ttl) {
                let teams = storage.list_teams()?;
                if !teams.is_empty() {
                    let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
                    let page_limit = paged_list_limit(limit, None, json_out, teams.len());
                    let page = paginate(teams, page_limit, None, stamp)?;
                    return print_teams_output(&page, None, json_out);
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let force_fetch = refresh
        || (!cached
            && !daemon_warm
            && !(ttl > 0 && is_cursor_fresh(&storage, &["cli:teams"], ttl)));
    let (teams, channel_errors) = load_teams_with_refresh(&graph, &storage, force_fetch).await?;
    if let Some(ref errors) = channel_errors {
        for err in errors {
            eprintln!(
                "Warning: could not refresh channels for team \"{}\" ({}): {}. Cached channels are kept.",
                err.display_name, err.team_id, err.error
            );
        }
    }
    let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
    let page_limit = paged_list_limit(limit, None, json_out, teams.len());
    let page = paginate(teams, page_limit, None, stamp)?;
    print_teams_output(&page, channel_errors.as_deref(), json_out)
}

async fn cmd_channels(
    cfg: &AppConfig,
    team_query: &str,
    limit: Option<usize>,
    cursor: Option<&str>,
    refresh: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    if cursor.is_some() {
        let storage = open_storage_only(cfg)?;
        let teams = storage.list_teams()?;
        let team = resolve_team_from_slice(&teams, team_query)?;
        let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
        let page_limit = paged_list_limit(limit, cursor, json_out, team.channels.len());
        let page = paginate(team.channels.clone(), page_limit, cursor, stamp)?;
        return print_channels_output(&team, &page, json_out);
    }

    let ttl = cli_cache_ttl_secs(cfg);
    let daemon_warm = resolve_daemon_warm(cfg, refresh, cached).await;
    if !refresh && (cached || daemon_warm || ttl > 0) {
        if let Ok(storage) = open_storage_only(cfg) {
            if cached || daemon_warm || is_cursor_fresh(&storage, &["cli:teams"], ttl) {
                let teams = storage.list_teams()?;
                if !teams.is_empty() {
                    if let Ok(team) = resolve_team_from_slice(&teams, team_query) {
                        let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
                        let page_limit =
                            paged_list_limit(limit, None, json_out, team.channels.len());
                        let page = paginate(team.channels.clone(), page_limit, None, stamp)?;
                        return print_channels_output(&team, &page, json_out);
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
    let (teams, channel_errors) = load_teams_with_refresh(&graph, &storage, force_fetch).await?;
    let mut team = resolve_team_from_slice(&teams, team_query)?;

    if let Some(err) = channel_errors
        .as_deref()
        .and_then(|errs| errs.iter().find(|e| e.team_id == team.id))
    {
        bail!(
            "Could not list channels for team \"{}\" ({}): {}",
            err.display_name,
            err.team_id,
            err.error
        );
    }

    let channels = if refresh && team.channels.is_empty() {
        let fetched = graph.list_team_channels(&team.id, &team.display_name).await?;
        if !fetched.is_empty() {
            team.channels = fetched.clone();
            let _ = storage.upsert_teams(std::slice::from_ref(&team));
        }
        fetched
    } else {
        team.channels.clone()
    };

    let stamp = fetch_stamp(&storage, "cli:teams").map_or(0, |(s, _)| s);
    let page_limit = paged_list_limit(limit, None, json_out, channels.len());
    let page = paginate(channels, page_limit, None, stamp)?;
    print_channels_output(&team, &page, json_out)
}

async fn load_teams_with_refresh(
    graph: &GraphClient,
    storage: &Storage,
    refresh: bool,
) -> Result<(Vec<TeamSummary>, Option<Vec<TeamChannelError>>)> {
    let cached = storage.list_teams()?;
    if refresh || cached.is_empty() {
        let fetched = graph.list_joined_teams_with_channels().await?;
        let merged = storage.upsert_joined_teams(&fetched)?;
        record_cli_sync_cursor(storage, "cli:teams");
        let stored = storage.list_teams()?;
        let teams = if stored.is_empty() { merged } else { stored };
        Ok((teams, Some(fetched.channel_errors)))
    } else {
        Ok((cached, None))
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

struct ReadOutput {
    json: bool,
    compact: bool,
    /// Arguments to repeat in the --full hint, e.g. " --limit 30 --before c-msg-3 --offline".
    full_args: String,
}

fn format_full_args(limit: usize, before: Option<&str>, offline: bool) -> String {
    let mut out = format!(" --limit {}", clamp_limit(limit));
    if let Some(b) = before.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!(" --before {b}"));
    }
    if offline {
        out.push_str(" --offline");
    }
    out
}

fn print_read_output(
    conv: &ConversationSummary,
    res: &ReadMessagesResult,
    output: &ReadOutput,
) -> Result<()> {
    if output.json {
        let hint = output.compact.then(|| {
            format!(
                "{} read {} --json --full{}",
                Persona::detect().bin_name(),
                conv.conv_key,
                output.full_args
            )
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&read_messages_json(conv, res, hint.as_deref()))?
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
        return print_read_output(&conv, &res, &output);
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
                            return print_read_output(&conv, &res, &output);
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
                            let is_eof = storage
                                .get_sync_cursor(&SyncCursor::history_scope(&conv.conv_key))
                                .ok()
                                .flatten()
                                .is_some_and(|c| c.is_history_eof());
                            if res.messages.len() >= limit || is_eof {
                                return print_read_output(&conv, &res, &output);
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

    print_read_output(&conv, &res, &output)
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
        let (thread, coverage) = read_thread_offline(&storage, &conv, root_msg_id, limit)?
            .ok_or_else(|| thread_cache_miss(root_msg_id, limit))?;
        return print_thread_output(&conv, &thread, coverage, &output);
    }
    if cached {
        if let Ok(storage) = open_storage_only(cfg) {
            if let Ok(cached_convs) = storage.list_conversations() {
                if let Ok(conv) = resolver::resolve_from_slice(&cached_convs, channel_target) {
                    if let Some((thread, coverage)) =
                        read_thread_offline(&storage, &conv, root_msg_id, limit)?
                    {
                        return print_thread_output(&conv, &thread, coverage, &output);
                    }
                }
            }
        }
    }

    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, channel_target, false).await?;
    let (thread, coverage) = match read_thread(&storage, &graph, &conv, root_msg_id, limit).await {
        Ok(found) => found,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };
    print_thread_output(&conv, &thread, coverage, &output)
}

fn print_thread_output(
    conv: &ConversationSummary,
    thread: &ThreadView,
    coverage: ThreadCoverage,
    output: &ReadOutput,
) -> Result<()> {
    if output.json {
        let hint = output.compact.then(|| {
            format!(
                "{} thread {} {} --json --full{}",
                Persona::detect().bin_name(),
                conv.conv_key,
                thread.root.msg_id,
                output.full_args
            )
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&thread_json(conv, thread, coverage, hint.as_deref()))?
        );
    } else {
        println!("# {} ({}) — Thread {}", conv.title, conv.conv_key, thread.root.msg_id);
        let mut display_root = thread.root.clone();
        display_root.reply_count = coverage.reply_total.unwrap_or(0);
        print_message_line(&display_root, "");
        if thread.replies.is_empty() {
            println!("  (No replies)");
        } else {
            for reply in &thread.replies {
                print_message_line(reply, "  ↳ ");
            }
        }
        if coverage.has_more {
            match coverage.reply_total {
                Some(total) => println!(
                    "  (Showing the newest {} of {} replies. Raise --limit to read more.)",
                    thread.replies.len(),
                    total
                ),
                None => println!(
                    "  (Showing the newest {} replies; more exist. Raise --limit, up to 1000, to read more.)",
                    thread.replies.len()
                ),
            }
        }
    }
    Ok(())
}

fn format_message_line(msg: &MessageItem, indent: &str) -> String {
    if msg.deleted {
        let tag_str = if msg.reply_count > 0 {
            format!(" [💬 {}]", msg.reply_count)
        } else {
            String::new()
        };
        return format!(
            "{indent}[{}] {} ({}){tag_str}: (deleted message)",
            msg.created_at, msg.author_name, msg.msg_id
        );
    }

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
            .filter(|a| !a.is_message_reference())
            .map(|a| {
                a.name
                    .clone()
                    .unwrap_or_else(|| a.content_type.clone())
            })
            .collect();
        if !names.is_empty() {
            tags.push(format!("[📎 {}]", names.join(", ")));
        }
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
    full: bool,
) -> Result<()> {
    if markdown.trim().is_empty() {
        bail!("Message body is empty.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_post_target(&storage, &graph, target).await?;
    let mentions = parse_mentions_cli(mentions_raw)?;
    let requested_mentions = mentions.clone();

    let mut outgoing =
        parser::compile_outgoing_markdown(markdown, subject, mentions, vec![], vec![], None);
    ensure_mentions_used(&requested_mentions, &outgoing.mentions)?;
    if important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = graph.send_message(&conv.target, &outgoing).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&sent));

    if json_out {
        let payload = if full {
            json!({
                "ok": true,
                "conversation": conv,
                "message": sent,
            })
        } else {
            sent_ack_json(&conv, &sent)
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
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
    full: bool,
) -> Result<()> {
    if markdown.trim().is_empty() {
        bail!("Reply body is empty.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_target(&storage, &graph, target, false).await?;
    let mentions = parse_mentions_cli(mentions_raw)?;
    let requested_mentions = mentions.clone();

    let quote = build_quote_reply(&storage, &graph, &conv, msg_id).await;
    let mut outgoing =
        parser::compile_outgoing_markdown(markdown, None, mentions, vec![], vec![], quote);
    ensure_mentions_used(&requested_mentions, &outgoing.mentions)?;
    if important {
        outgoing.importance = Some("high".to_string());
    }

    let sent = graph.reply_message(&conv.target, msg_id, &outgoing).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&sent));

    if json_out {
        let payload = if full {
            json!({
                "ok": true,
                "conversation": conv,
                "reply_to": msg_id,
                "message": sent,
            })
        } else {
            let mut ack = sent_ack_json(&conv, &sent);
            ack["reply_to"] = json!(msg_id);
            ack
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
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
    full: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let conv = resolve_post_target(&storage, &graph, target).await?;
    let sent = send_file_or_inline(&graph, &storage, &conv, path, caption, thread, inline).await?;

    if json_out {
        let payload = if full {
            json!({
                "ok": true,
                "conversation": conv,
                "path": path,
                "inline": inline,
                "message": sent,
            })
        } else {
            let mut ack = sent_ack_json(&conv, &sent);
            ack["path"] = json!(path);
            ack["inline"] = json!(inline);
            ack
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
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

/// One complete Graph fetch: replaces the cached member list, then records the completion marker.
pub(crate) async fn refresh_members(
    storage: &Storage,
    graph: &GraphClient,
    conv: &ConversationSummary,
) -> Result<()> {
    let fetched = graph.list_members(&conv.target).await?;
    storage
        .replace_members(&conv.conv_key, &fetched)
        .context("Graph returned the members, but updating the local cache failed")?;
    record_cli_sync_cursor(storage, &format!("cli:members:{}", conv.conv_key));
    Ok(())
}

/// Reads one page from the cached member list.
pub(crate) fn members_page(
    storage: &Storage,
    conv: &ConversationSummary,
    limit: usize,
    cursor: Option<&str>,
) -> Result<serde_json::Value> {
    let marker = fetch_stamp(storage, &format!("cli:members:{}", conv.conv_key));
    let members = storage.list_members(&conv.conv_key)?;
    if marker.is_none() && members.is_empty() {
        bail!(
            "Members of {} have not been fetched yet. Read the members online first.",
            conv.conv_key
        );
    }
    let stamp = marker.as_ref().map_or(0, |(stamp, _)| *stamp);
    let page = paginate(members, limit, cursor, stamp)?;
    let known = marker.is_some();
    let returned = page.items.len();
    Ok(json!({
        "conv_key": conv.conv_key,
        "title": conv.title,
        "members": page.items,
        "returned": returned,
        "has_more": if page.has_more || known { json!(page.has_more) } else { serde_json::Value::Null },
        "next_cursor": page.next_cursor,
        "total": if known { json!(page.total) } else { serde_json::Value::Null },
        "fetched_at": marker.map(|(_, at)| at),
    }))
}

async fn cmd_members(
    cfg: &AppConfig,
    target: &str,
    limit: Option<usize>,
    cursor: Option<&str>,
    offline: bool,
    json_out: bool,
) -> Result<()> {
    let from_cache = offline || cursor.is_some();
    let (_auth, graph, storage) = open_runtime(cfg, !from_cache).await?;
    let conv = resolve_target(&storage, &graph, target, from_cache).await?;

    if !from_cache {
        refresh_members(&storage, &graph, &conv).await?;
    }

    let page_limit = if json_out {
        clamp_limit(limit.unwrap_or(50))
    } else if limit.is_none() && cursor.is_none() {
        1_000
    } else {
        clamp_limit(limit.unwrap_or(50))
    };

    let payload = members_page(&storage, &conv, page_limit, cursor)?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        let members: Vec<MemberInfo> =
            serde_json::from_value(payload["members"].clone()).unwrap_or_default();
        if members.is_empty() {
            println!("No member information available.");
        } else {
            let returned = payload["returned"].as_u64().unwrap_or(members.len() as u64);
            let header_count = payload["total"].as_u64().unwrap_or(returned);
            println!("# {} ({}) Members ({})", conv.title, conv.conv_key, header_count);
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
            if let Some(next) = payload["next_cursor"].as_str() {
                match payload["total"].as_u64() {
                    Some(total) => println!(
                        "(Showing {} of {} members. Next page: --cursor {next})",
                        returned, total
                    ),
                    None => println!(
                        "(Showing {} members. Next page: --cursor {next})",
                        returned
                    ),
                }
            }
        }
    }
    Ok(())
}

async fn cmd_users(
    cfg: &AppConfig,
    query: &str,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    let q = query.trim();
    if q.is_empty() {
        bail!("Search query is empty.");
    }
    let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
    let users = match graph.search_users(q, clamp_limit(limit)).await {
        Ok(users) => users,
        Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
    };

    if json_out {
        println!("{}", serde_json::to_string_pretty(&users_search_json(&users))?);
    } else if users.is_empty() {
        println!("No matching users found.");
    } else {
        for u in &users {
            if let Some(addr) = u.mail.as_deref().or(u.user_principal_name.as_deref()) {
                println!("{}  <{addr}>  {}", u.display_name, u.id);
            } else {
                println!("{}  {}", u.display_name, u.id);
            }
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

    let mut errors = Vec::new();
    let targets: Vec<ConversationSummary> = match target {
        Some(t) => vec![resolve_target(&storage, &graph, t, false).await?],
        None => {
            let (chats_res, teams_res) = tokio::join!(
                graph.list_chats_with_cursor(chats_limit),
                graph.list_joined_teams_with_channels(),
            );
            let fetched_chats = match chats_res {
                Ok(c) => c,
                Err(err) => return Err(wrap_graph_permission_error(&graph, err).await),
            };
            storage.upsert_conversations_with_null_topics(
                &fetched_chats.chats,
                &fetched_chats.explicit_null_topic_keys,
            )?;
            record_chats_list_cursor(&storage, fetched_chats.next_link, fetched_chats.eof);
            match teams_res {
                Ok(fetched) => {
                    let _ = storage.upsert_joined_teams(&fetched)?;
                }
                Err(err) => {
                    let wrapped = wrap_graph_permission_error(&graph, err).await;
                    errors.push(format!("teams/channels: {wrapped:#}"));
                }
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
    // Roots whose replies could not be fetched: (root_msg_id, error). Their cached replies
    // are exported instead, and the command reports the export as incomplete.
    let mut incomplete_threads: Vec<(String, String)> = Vec::new();
    let mut thread_roots_total = 0usize;
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
            thread_roots_total = root_ids.len();

            if offline {
                for msg_id in root_ids {
                    let replies = storage.list_thread_replies(&conv.conv_key, msg_id, limit)?;
                    if !replies.is_empty() {
                        threads_by_root.insert(msg_id.to_string(), replies);
                    }
                }
            } else {
                let fetched: Vec<(String, Result<Vec<MessageItem>>)> = stream::iter(root_ids)
                    .map(|msg_id| {
                        let graph = &graph;
                        let storage = &storage;
                        async move {
                            let r = graph
                                .list_thread_replies(team_id, channel_id, msg_id, limit)
                                .await;
                            if let Ok(replies) = &r {
                                if !replies.is_empty() {
                                    let _ = storage.upsert_messages(replies);
                                }
                            }
                            (msg_id.to_string(), r)
                        }
                    })
                    .buffer_unordered(6)
                    .collect()
                    .await;

                for (msg_id, result) in fetched {
                    let replies = match result {
                        Ok(replies) => replies,
                        Err(err) => {
                            incomplete_threads.push((msg_id.clone(), format!("{err:#}")));
                            storage
                                .list_thread_replies(&conv.conv_key, &msg_id, limit)
                                .unwrap_or_default()
                        }
                    };
                    if !replies.is_empty() {
                        threads_by_root.insert(msg_id, replies);
                    }
                }
                // Deterministic report order regardless of fetch completion order.
                incomplete_threads.sort();
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
                let mut doc = json!({
                    "conversation": conv,
                    "items": entries,
                });
                // Present only for a partial export, so a clean one is byte-identical.
                if !incomplete_threads.is_empty() {
                    doc["incomplete_threads"] = incomplete_threads
                        .iter()
                        .map(|(root_msg_id, error)| {
                            json!({ "root_msg_id": root_msg_id, "error": error })
                        })
                        .collect();
                }
                serde_json::to_string_pretty(&doc)?
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
                let plain = if m.deleted {
                    "(deleted message)".to_string()
                } else if !m.body_plain.trim().is_empty() {
                    m.body_plain.trim().to_string()
                } else {
                    parser::html_and_cards_to_plain(&m.body_html, &m.attachments)
                };
                out_str.push_str(&format!("[{}] {}: {}\n", m.created_at, m.author_name, plain));
                if let Some(replies) = threads_by_root.get(&m.msg_id) {
                    for r in replies {
                        let rplain = if r.deleted {
                            "(deleted message)".to_string()
                        } else if !r.body_plain.trim().is_empty() {
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
                if m.deleted {
                    out_str.push_str(&format!(
                        "- **{}** `{}`\n\n  (deleted message)\n\n",
                        m.author_name, m.created_at
                    ));
                } else {
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
                }
                if let Some(replies) = threads_by_root.get(&m.msg_id) {
                    for r in replies {
                        let rtext = if r.deleted {
                            "(deleted message)".to_string()
                        } else {
                            let rmd = parser::html_and_cards_to_markdown(&r.body_html, &r.attachments);
                            if rmd.trim().is_empty() {
                                r.body_plain.trim().to_string()
                            } else {
                                rmd.trim().to_string()
                            }
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

    // The (partial) export is already written; now make the gap impossible to miss.
    if !incomplete_threads.is_empty() {
        for (root_msg_id, error) in &incomplete_threads {
            eprintln!("warning: thread {root_msg_id}: replies could not be fetched: {error}");
        }
        bail!(
            "export incomplete: {} of {} threads could not be fetched",
            incomplete_threads.len(),
            thread_roots_total
        );
    }
    Ok(())
}

pub(crate) async fn fetch_presence_after_set(
    graph: &GraphClient,
    availability: &str,
    activity: &str,
) -> (
    Option<crate::models::PresenceStatus>,
    Option<crate::models::PresenceStatus>,
) {
    let latest = graph.get_my_presence().await.ok();
    let my_uid = latest
        .as_ref()
        .map(|p| p.user_id.clone())
        .filter(|id| !id.is_empty())
        .or_else(|| graph.my_user_id().map(str::to_string));
    let my_uid = match my_uid {
        Some(id) if !id.is_empty() => Some(id),
        _ => graph
            .auth()
            .current_tokens()
            .await
            .and_then(|t| t.user_id)
            .filter(|id| !id.is_empty()),
    };
    let to_cache = if availability.eq_ignore_ascii_case("Reset") {
        latest.clone().filter(|p| !p.user_id.is_empty())
    } else {
        my_uid.map(|uid| {
            latest.clone().unwrap_or(crate::models::PresenceStatus {
                user_id: uid,
                availability: availability.to_string(),
                activity: activity.to_string(),
            })
        })
    };
    (latest, to_cache)
}

async fn cmd_presence(
    cfg: &AppConfig,
    set: Option<&str>,
    activity: Option<&str>,
    expiration_mins: Option<u32>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;

    if let Some(raw_avail) = set {
        let availability = normalize_presence_availability(raw_avail)?;
        let act = activity
            .map(|a| a.trim())
            .filter(|a| !a.is_empty())
            .unwrap_or(&availability);
        graph
            .set_my_presence(&availability, act, expiration_mins)
            .await?;

        let (latest, to_cache) = fetch_presence_after_set(&graph, &availability, act).await;
        if let Some(status) = to_cache {
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

pub(crate) struct Page<T> {
    pub items: Vec<T>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub total: usize,
}

/// Offset paging over a fully loaded list. A cursor is "<offset>-<stamp>"; the stamp ties it
/// to one completed fetch of one resource, so a cursor from another list or an older fetch is rejected.
pub(crate) fn paginate<T>(
    items: Vec<T>,
    limit: usize,
    cursor: Option<&str>,
    stamp: i64,
) -> Result<Page<T>> {
    let total = items.len();
    let offset = match cursor {
        None => 0,
        Some(raw) => {
            let (offset, cursor_stamp) = raw
                .trim()
                .split_once('-')
                .and_then(|(o, s)| Some((o.parse::<usize>().ok()?, s.parse::<i64>().ok()?)))
                .filter(|(offset, _)| *offset <= total)
                .with_context(|| {
                    format!(
                        "Invalid cursor: {raw}. Pass the next_cursor value from the previous page."
                    )
                })?;
            if cursor_stamp != stamp {
                bail!(
                    "Stale cursor: {raw}. The list was refreshed or belongs to another target; start again without a cursor."
                );
            }
            offset
        }
    };
    let end = offset.saturating_add(limit).min(total);
    let has_more = end < total;
    Ok(Page {
        items: items.into_iter().skip(offset).take(limit).collect(),
        has_more,
        next_cursor: has_more.then(|| format!("{end}-{stamp}")),
        total,
    })
}

/// Milliseconds of the completed-fetch marker for a scope; None when no complete fetch was recorded.
pub(crate) fn fetch_stamp(storage: &Storage, scope_key: &str) -> Option<(i64, String)> {
    let cursor = storage.get_sync_cursor(scope_key).ok().flatten()?;
    let at = chrono::DateTime::parse_from_rfc3339(&cursor.synced_at).ok()?;
    Some((at.timestamp_millis(), cursor.synced_at))
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

const COMPACT_OMITTED_FIELDS: &[&str] = &[
    "msg_key",
    "conv_key",
    "reply_to_author",
    "reply_to_preview",
    "is_me",
    "body_html",
    "web_url",
    "attachments.content_url",
    "attachments.content_json",
    "attachments.thumbnail_url",
    "conversation.target",
    "conversation.kind",
    "conversation.team_id",
    "conversation.team_name",
    "conversation.topic",
    "conversation.unread_count",
    "conversation.last_read_at",
    "conversation.last_msg_id",
    "conversation.last_msg_author",
    "conversation.last_msg_preview",
    "conversation.last_updated_at",
    "conversation.pinned",
    "conversation.peer_user_id",
    "conversation.web_url",
];

fn compact_attachment(att: &crate::models::AttachmentInfo) -> serde_json::Value {
    let mut stub = json!({ "id": att.id, "name": att.name });
    let content_type = att.content_type.trim();
    if !content_type.is_empty() {
        stub["content_type"] = json!(content_type);
    }
    stub
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
    if let Some(subj) = msg.subject.as_deref().filter(|s| !s.trim().is_empty()) {
        value["subject"] = json!(subj);
    }
    if msg.deleted {
        value["deleted"] = json!(true);
    }
    if msg.message_type != "message" {
        value["message_type"] = json!(msg.message_type);
    }
    if !msg.importance.eq_ignore_ascii_case("normal") {
        value["importance"] = json!(msg.importance);
    }
    if msg.reply_count > 0 {
        value["reply_count"] = json!(msg.reply_count);
    }
    let attachments: Vec<serde_json::Value> = msg
        .attachments
        .iter()
        .filter(|att| !att.is_message_reference())
        .map(compact_attachment)
        .collect();
    if !attachments.is_empty() {
        value["attachments"] = serde_json::Value::Array(attachments);
    }
    let links: Vec<serde_json::Value> =
        crate::ui::overlays::links::extract_links_from_text(&msg.body_html, "")
            .into_iter()
            .filter(|link| !msg.body_plain.contains(&link.url))
            .map(|link| json!({ "text": link.label, "url": link.url }))
            .collect();
    if !links.is_empty() {
        value["links"] = serde_json::Value::Array(links);
    }
    if !msg.mentions.is_empty() || !msg.reactions.is_empty() {
        value["omitted"] = json!({
            "mentions": msg.mentions.len(),
            "reactions": msg.reactions.len(),
        });
    }
    value
}

pub(crate) fn sent_ack_json(
    conv: &ConversationSummary,
    msg: &MessageItem,
) -> serde_json::Value {
    let mut conv_val = json!({
        "conv_key": conv.conv_key,
        "title": conv.title,
    });
    if conv.kind == ConversationKind::Channel {
        if let Some(team) = conv.team_name.as_deref().filter(|t| !t.is_empty()) {
            conv_val["team_name"] = json!(team);
        }
    }
    let mut msg_val = json!({
        "msg_id": msg.msg_id,
        "created_at": msg.created_at,
    });
    if !msg.mentions.is_empty() {
        msg_val["mentions"] = json!(msg.mentions.len());
    }
    let attachments: Vec<serde_json::Value> = msg
        .attachments
        .iter()
        .filter(|att| !att.is_message_reference())
        .map(compact_attachment)
        .collect();
    if !attachments.is_empty() {
        msg_val["attachments"] = serde_json::Value::Array(attachments);
    }
    json!({
        "ok": true,
        "conversation": conv_val,
        "message": msg_val,
    })
}

pub(crate) fn users_search_json(users: &[UserProfile]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = users
        .iter()
        .map(|u| {
            json!({
                "user_id": u.id,
                "display_name": u.display_name,
                "email": u.mail,
                "user_principal_name": u.user_principal_name,
            })
        })
        .collect();
    let returned = items.len();
    json!({
        "users": items,
        "returned": returned,
    })
}

pub(crate) fn finish_thread_value(
    mut thread: serde_json::Value,
    returned: usize,
    coverage: ThreadCoverage,
) -> serde_json::Value {
    thread["root"]["reply_count"] = json!(coverage.reply_total);
    thread["returned"] = json!(returned);
    thread["has_more"] = json!(coverage.has_more);
    thread
}

pub(crate) fn read_messages_json(
    conv: &ConversationSummary,
    res: &ReadMessagesResult,
    hint: Option<&str>,
) -> serde_json::Value {
    if let Some(hint) = hint {
        let omitted_mentions: usize = res.messages.iter().map(|m| m.mentions.len()).sum();
        let omitted_reactions: usize = res.messages.iter().map(|m| m.reactions.len()).sum();
        json!({
            "conversation": compact_conversation(conv),
            "messages": serde_json::Value::Array(res.messages.iter().map(compact_message).collect()),
            "has_more": res.has_more,
            "next_before": res.next_before,
            "omitted": {
                "mentions": omitted_mentions,
                "reactions": omitted_reactions,
                "fields": COMPACT_OMITTED_FIELDS,
            },
            "hint": hint,
        })
    } else {
        json!({
            "conversation": conv,
            "messages": res.messages,
            "has_more": res.has_more,
            "next_before": res.next_before,
        })
    }
}

pub(crate) fn thread_json(
    conv: &ConversationSummary,
    thread: &ThreadView,
    coverage: ThreadCoverage,
    hint: Option<&str>,
) -> serde_json::Value {
    if let Some(hint) = hint {
        let omitted_mentions: usize = std::iter::once(&thread.root)
            .chain(&thread.replies)
            .map(|m| m.mentions.len())
            .sum();
        let omitted_reactions: usize = std::iter::once(&thread.root)
            .chain(&thread.replies)
            .map(|m| m.reactions.len())
            .sum();
        let replies: Vec<_> = thread.replies.iter().map(compact_message).collect();
        let thread_val = finish_thread_value(
            json!({ "root": compact_message(&thread.root), "replies": replies }),
            thread.replies.len(),
            coverage,
        );
        json!({
            "conversation": compact_conversation(conv),
            "thread": thread_val,
            "omitted": {
                "mentions": omitted_mentions,
                "reactions": omitted_reactions,
                "fields": COMPACT_OMITTED_FIELDS,
            },
            "hint": hint,
        })
    } else {
        let thread_val = finish_thread_value(json!(thread), thread.replies.len(), coverage);
        json!({ "conversation": conv, "thread": thread_val })
    }
}

pub(crate) fn read_thread_offline(
    storage: &Storage,
    conv: &ConversationSummary,
    root_msg_id: &str,
    limit: usize,
) -> Result<Option<(ThreadView, ThreadCoverage)>> {
    if !matches!(conv.target, ConversationTarget::Channel { .. }) {
        bail!("Thread reads require a team channel target: {}", conv.conv_key);
    }
    let mut found = storage.get_fetched_thread(&conv.conv_key, root_msg_id, limit)?;
    if let Some((view, _)) = found.as_mut() {
        hydrate_thread_attachments(view);
    }
    Ok(found)
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
) -> Result<(ThreadView, ThreadCoverage)> {
    let ConversationTarget::Channel { team_id, channel_id } = &conv.target else {
        bail!("Thread reads require a team channel target: {}", conv.conv_key);
    };
    let (mut thread, complete) = graph
        .get_thread_with_coverage(team_id, channel_id, root_msg_id, limit)
        .await?;
    storage.save_fetched_thread(&thread, complete)?;
    hydrate_thread_attachments(&mut thread);
    let coverage = ThreadCoverage {
        has_more: !complete,
        reply_total: complete.then_some(thread.replies.len()),
    };
    Ok((thread, coverage))
}

#[derive(Debug)]
struct DetailedError {
    message: String,
    details: serde_json::Value,
}

impl std::fmt::Display for DetailedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DetailedError {}

fn is_forbidden_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    // The parsed response status, not a "403" substring: URLs and message ids contain digits.
    extract_graph_http_status(msg) == Some(403)
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("insufficient privileges")
        || lower.contains("missing scope")
        || lower.contains("aclcheckfailed")
        || lower.contains("insufficient graph permissions")
        || lower.contains("insufficient permissions")
}

pub(crate) fn is_forbidden_error(err: &anyhow::Error) -> bool {
    is_forbidden_message(&format!("{err:#}"))
}

pub(crate) use crate::graph::extract_graph_http_status;

pub(crate) fn error_code_for_message(msg: &str) -> &'static str {
    if msg.contains(crate::http::TLS_UNTRUSTED_MARKER) {
        "tls_untrusted_certificate"
    } else if msg.contains("Conversation/channel not found") {
        "target_not_found"
    } else if msg.contains("Sign-in required") {
        "not_signed_in"
    } else if msg.contains("Token expired") {
        "token_expired"
    } else if is_forbidden_message(msg) {
        "permission_denied"
    } else {
        match extract_graph_http_status(msg) {
            Some(401) => "unauthorized",
            Some(404) => "not_found",
            _ => "error",
        }
    }
}

pub(crate) fn error_json_from_message(msg: &str) -> serde_json::Value {
    let code = error_code_for_message(msg);
    let http_status = if code == "permission_denied" {
        Some(403)
    } else {
        extract_graph_http_status(msg)
    };
    let mut err_obj = json!({
        "code": code,
        "message": msg,
    });
    if let Some(status) = http_status {
        err_obj["http_status"] = json!(status);
    }
    json!({
        "ok": false,
        "error": err_obj,
    })
}

pub(crate) fn error_json(err: &anyhow::Error) -> serde_json::Value {
    let message = format!("{err:#}");
    let mut err_obj = if let Some(amb) = err.downcast_ref::<resolver::AmbiguousTarget>() {
        json!({
            "code": "target_ambiguous",
            "message": message,
            "candidates": amb.candidates,
        })
    } else {
        let code = error_code_for_message(&message);
        let http_status = if code == "permission_denied" {
            Some(403)
        } else {
            extract_graph_http_status(&message)
        };
        let mut obj = json!({
            "code": code,
            "message": message,
        });
        if let Some(status) = http_status {
            obj["http_status"] = json!(status);
        }
        if let Some(unv) = err.downcast_ref::<resolver::UnverifiedTarget>() {
            obj["candidates"] = json!(unv.candidates);
        }
        if let Some(det) = err.downcast_ref::<DetailedError>() {
            obj["details"] = det.details.clone();
        }
        obj
    };
    if let Some(det) = err.downcast_ref::<DetailedError>() {
        if err_obj.get("details").is_none() {
            err_obj["details"] = det.details.clone();
        }
    }
    json!({
        "ok": false,
        "error": err_obj,
    })
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
        return Ok((
            crate::storage::db::normalize_timestamp_sort_key(&msg.created_at),
            Some(msg.msg_id),
        ));
    }

    let ts = normalize_before_timestamp(trimmed)?;
    Ok((ts, None))
}

fn normalize_before_timestamp(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Nanos, true));
    }
    if let Ok(date) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        let dt = date
            .and_hms_opt(0, 0, 0)
            .context("Failed to construct midnight UTC timestamp")?
            .and_utc();
        return Ok(dt.to_rfc3339_opts(SecondsFormat::Nanos, true));
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
                .to_rfc3339_opts(SecondsFormat::Nanos, true));
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
    if msg.deleted {
        return;
    }
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
            let oldest_ts = if next_link.is_none() {
                Some(SyncCursor::HISTORY_EOF.to_string())
            } else {
                fetched
                    .iter()
                    .filter(|m| m.parent_msg_id.is_none())
                    .map(|m| crate::storage::db::normalize_timestamp_sort_key(&m.created_at))
                    .filter(|s| !s.is_empty())
                    .min()
            };
            if oldest_ts.is_some() {
                // A plain read only sees the newest page: it must not replace a cursor that
                // an earlier `--before` walk or the TUI already moved deeper (or to EOF).
                let _ = storage.save_history_cursor_if_deeper(&SyncCursor {
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
                let local = storage.list_older_root_messages(
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
                                .map(|m| crate::storage::db::normalize_timestamp_sort_key(&m.created_at))
                                .filter(|s| !s.is_empty())
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
                    let cursor_connected = cursor.as_ref().is_some_and(|c| {
                        !c.is_history_eof()
                            && c.delta_link.is_some()
                            && c.oldest_msg_ts.as_deref().is_some_and(|ots| {
                                let norm_ots =
                                    crate::storage::db::normalize_timestamp_sort_key(ots);
                                !norm_ots.is_empty() && norm_ots >= before_ts
                            })
                    });
                    let mut next_cursor_link = if cursor_connected {
                        cursor.as_ref().and_then(|c| c.delta_link.clone())
                    } else {
                        None
                    };
                    let mut fetched_older_ids = std::collections::HashSet::new();

                    loop {
                        let prev_link = next_cursor_link.clone();
                        let remaining = limit.saturating_sub(fetched_older_ids.len()).max(1);
                        let (fetched, new_next_link) = match graph
                            .fetch_older_messages(
                                &conv.target,
                                prev_link.as_deref(),
                                Some(&before_ts),
                                remaining,
                            )
                            .await
                        {
                            Ok(res) => res,
                            Err(err) => return Err(wrap_graph_permission_error(graph, err).await),
                        };

                        if !fetched.is_empty() {
                            for m in &fetched {
                                if m.parent_msg_id.is_none() {
                                    let m_sort = crate::storage::db::normalize_timestamp_sort_key(
                                        &m.created_at,
                                    );
                                    let is_older = match before_id.as_deref() {
                                        Some(bid) => {
                                            (m_sort.as_str(), m.msg_id.as_str())
                                                < (before_ts.as_str(), bid)
                                        }
                                        None => m_sort.as_str() < before_ts.as_str(),
                                    };
                                    if is_older {
                                        fetched_older_ids.insert(m.msg_id.clone());
                                    }
                                }
                            }
                            storage.upsert_messages(&fetched)?;
                        }
                        let oldest_ts = if new_next_link.is_none() {
                            Some(SyncCursor::HISTORY_EOF.to_string())
                        } else {
                            fetched
                                .iter()
                                .filter(|m| m.parent_msg_id.is_none())
                                .map(|m| {
                                    crate::storage::db::normalize_timestamp_sort_key(&m.created_at)
                                })
                                .filter(|s| !s.is_empty())
                                .min()
                        };
                        if oldest_ts.is_some() {
                            let _ = storage.save_sync_cursor(&SyncCursor {
                                scope_key: scope_key.clone(),
                                delta_link: new_next_link.clone(),
                                oldest_msg_ts: oldest_ts,
                                synced_at: Utc::now().to_rfc3339(),
                            });
                        }

                        next_cursor_link = new_next_link;
                        if fetched_older_ids.len() >= limit
                            || next_cursor_link.is_none()
                            || (fetched.is_empty() && next_cursor_link == prev_link)
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

pub(crate) async fn resolve_post_target(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
) -> Result<ConversationSummary> {
    resolver::resolve_post_target(storage, graph, query).await
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

pub(crate) fn ensure_mentions_used(
    requested: &[MentionInfo],
    used: &[MentionInfo],
) -> Result<()> {
    let missing: Vec<&MentionInfo> = requested
        .iter()
        .filter(|req| !used.iter().any(|u| u.user_id == req.user_id))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let list = missing
        .iter()
        .map(|m| format!("@{} ({})", m.display_name, m.user_id))
        .collect::<Vec<_>>()
        .join(", ");
    let tokens = missing
        .iter()
        .map(|m| format!("@{}", m.display_name))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "Mention not found in message body: {list}. Write {tokens} in the text where the mention should appear."
    );
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
        None => match graph.get_message(&conv.target, msg_id, None).await {
            Ok(m) => {
                let _ = storage.upsert_messages(std::slice::from_ref(&m));
                Some(m)
            }
            Err(_) => None,
        },
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

    let parent_id = match &conv.target {
        ConversationTarget::Channel { .. } => {
            thread_root_id.map(str::trim).filter(|s| !s.is_empty())
        }
        ConversationTarget::Chat { .. } => None,
    };
    let msg = graph.get_message(&conv.target, msg_id, parent_id).await?;
    let _ = storage.upsert_messages(std::slice::from_ref(&msg));
    Ok(msg)
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
                full,
            } => {
                assert_eq!(target, "Engineering/General");
                assert_eq!(limit, 30);
                assert!(before.is_none());
                assert!(thread.is_none());
                assert!(!offline);
                assert!(!cached);
                assert!(!json);
                assert!(!compact);
                assert!(!full);
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
                assert!(thread.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "teams",
            "read",
            "chat:1",
            "--thread",
            "root-1",
            "--limit",
            "100",
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
                assert!(before.is_none());
                assert_eq!(thread.as_deref(), Some("root-1"));
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let conflict = Cli::try_parse_from([
            "teams",
            "read",
            "chat:1",
            "--before",
            "2026-11-01",
            "--thread",
            "root-1",
        ])
        .unwrap_err();
        assert_eq!(conflict.kind(), clap::error::ErrorKind::ArgumentConflict);

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
                full: false,
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
                full: false,
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
        assert!(Cli::try_parse_from(["teams", "read", "chat:c", "--json", "--full"]).is_ok());
        assert!(Cli::try_parse_from([
            "teams", "read", "chat:c", "--json", "--compact", "--full",
        ])
        .is_err());
        assert!(Cli::try_parse_from(["teams", "read", "chat:c", "--full"]).is_err());
        assert!(Cli::try_parse_from([
            "teams", "thread", "chan:t:c", "m1", "--json", "--full",
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["teams", "send", "chat:c", "hi", "--full"]).is_err());
        assert!(Cli::try_parse_from(["teams", "send", "chat:c", "hi", "--json", "--full"]).is_ok());
        assert!(Cli::try_parse_from(["teams", "reply", "chat:c", "m1", "hi", "--full"]).is_err());
        assert!(Cli::try_parse_from([
            "teams", "reply", "chat:c", "m1", "hi", "--json", "--full",
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "teams", "send-file", "chat:c", "a.txt", "--full",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "teams", "send-file", "chat:c", "a.txt", "--json", "--full",
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["teams", "users", "Alice", "--limit", "5", "--json"]).is_ok());
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
            Command::Serve {
                bind,
                port,
                token,
                allow_keyless,
                cors_origin,
            } => {
                assert_eq!(bind, "0.0.0.0");
                assert_eq!(port, 9000);
                assert_eq!(token.as_deref(), Some("sec"));
                assert!(!allow_keyless);
                assert!(cors_origin.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }

        let cli = Cli::try_parse_from([
            "teams",
            "serve",
            "--allow-keyless",
            "--cors-origin",
            "http://localhost:3000",
            "--cors-origin",
            "https://app.example",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Serve {
                token,
                allow_keyless,
                cors_origin,
                ..
            } => {
                assert!(token.is_none());
                assert!(allow_keyless);
                assert_eq!(cors_origin, ["http://localhost:3000", "https://app.example"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["teams", "serve", "--allow-keyless", "--token", "sec"]).is_err(),
            "--allow-keyless and --token are mutually exclusive"
        );

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

        // Date-only `YYYY-MM-DD` normalizes to fixed-width UTC 9-digit comparison key
        let (ts, id) = resolve_before_cursor(&storage, &conv.conv_key, "2026-11-01").unwrap();
        assert_eq!(ts, "2026-11-01T00:00:00.000000000Z");
        assert!(id.is_none());

        // Cached msg_id resolves to its normalized timestamp key and msg_id
        let (ts, id) = resolve_before_cursor(&storage, &conv.conv_key, "m3").unwrap();
        assert_eq!(ts, "2026-11-01T09:00:00.000000000Z");
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

    #[test]
    fn parses_hyphen_text_values_across_affected_commands() {
        let ko = "- 목록 항목";
        let multiline = "- first\n- second";

        // 1. send chat X (+ alias `msg`, multiline, subject, trailing --json)
        match Cli::try_parse_from(["teams", "send", "chat:c1", ko, "--json"])
            .unwrap()
            .command
            .unwrap()
        {
            Command::Send {
                target,
                markdown,
                json,
                ..
            } => {
                assert_eq!(target, "chat:c1");
                assert_eq!(markdown, ko);
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match Cli::try_parse_from([
            "teams",
            "msg",
            "chat:c1",
            multiline,
            "--subject",
            ko,
            "--mention",
            "u1:Alice",
            "--important",
            "--json",
        ])
        .unwrap()
        .command
        .unwrap()
        {
            Command::Send {
                target,
                markdown,
                subject,
                mention,
                important,
                json,
                full: false,
            } => {
                assert_eq!(target, "chat:c1");
                assert_eq!(markdown, multiline);
                assert_eq!(subject.as_deref(), Some(ko));
                assert_eq!(mention, vec!["u1:Alice"]);
                assert!(important);
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }
        // send chat body --subject X
        match Cli::try_parse_from(["teams", "send", "chat:c1", "body", "--subject", ko, "--json"])
            .unwrap()
            .command
            .unwrap()
        {
            Command::Send {
                target,
                markdown,
                subject,
                json,
                ..
            } => {
                assert_eq!(target, "chat:c1");
                assert_eq!(markdown, "body");
                assert_eq!(subject.as_deref(), Some(ko));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // 2. reply chat id X
        match Cli::try_parse_from([
            "teams", "reply", "chat:c1", "m1", multiline, "--mention", "u1:Alice", "--json",
        ])
        .unwrap()
        .command
        .unwrap()
        {
            Command::Reply {
                target,
                msg_id,
                markdown,
                mention,
                json,
                ..
            } => {
                assert_eq!(target, "chat:c1");
                assert_eq!(msg_id, "m1");
                assert_eq!(markdown, multiline);
                assert_eq!(mention, vec!["u1:Alice"]);
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // 3. edit chat id X, edit --find/--replace, edit --subject
        match Cli::try_parse_from(["teams", "edit", "chat:c1", "m1", ko, "--json"])
            .unwrap()
            .command
            .unwrap()
        {
            Command::Edit {
                target,
                msg_id,
                new_markdown,
                json,
                ..
            } => {
                assert_eq!(target, "chat:c1");
                assert_eq!(msg_id, "m1");
                assert_eq!(new_markdown.as_deref(), Some(ko));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match Cli::try_parse_from([
            "teams", "edit", "chat:c1", "m1", "--find", "word", "--replace", ko, "--json",
        ])
        .unwrap()
        .command
        .unwrap()
        {
            Command::Edit {
                find,
                replace,
                json,
                ..
            } => {
                assert_eq!(find.as_deref(), Some("word"));
                assert_eq!(replace.as_deref(), Some(ko));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match Cli::try_parse_from([
            "teams", "edit", "chat:c1", "m1", "--find", ko, "--replace", "word", "--json",
        ])
        .unwrap()
        .command
        .unwrap()
        {
            Command::Edit {
                find,
                replace,
                json,
                ..
            } => {
                assert_eq!(find.as_deref(), Some(ko));
                assert_eq!(replace.as_deref(), Some("word"));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match Cli::try_parse_from([
            "teams", "edit", "chan:t1:c1", "m1", "--subject", ko, "--json",
        ])
        .unwrap()
        .command
        .unwrap()
        {
            Command::Edit { subject, json, .. } => {
                assert_eq!(subject.as_deref(), Some(ko));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // 4. new-chat --message and --topic
        match Cli::try_parse_from([
            "teams",
            "new-chat",
            "--to",
            "audit@example.invalid",
            "--topic",
            ko,
            "--message",
            multiline,
            "--json",
        ])
        .unwrap()
        .command
        .unwrap()
        {
            Command::NewChat {
                to,
                topic,
                message,
                json,
            } => {
                assert_eq!(to, vec!["audit@example.invalid"]);
                assert_eq!(topic.as_deref(), Some(ko));
                assert_eq!(message.as_deref(), Some(multiline));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // 5. update-chat --topic
        match Cli::try_parse_from(["teams", "update-chat", "chat:c1", "--topic", ko, "--json"])
            .unwrap()
            .command
            .unwrap()
        {
            Command::UpdateChat {
                target,
                topic,
                json,
            } => {
                assert_eq!(target, "chat:c1");
                assert_eq!(topic, ko);
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // 6. search X (with trailing options including --top alias)
        match Cli::try_parse_from(["teams", "search", ko, "--top", "15", "--json"])
            .unwrap()
            .command
            .unwrap()
        {
            Command::Search {
                query, limit, json, ..
            } => {
                assert_eq!(query, ko);
                assert_eq!(limit, 15);
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // 7. send-file --caption (and alias `file`)
        match Cli::try_parse_from(["teams", "file", "chat:c1", "shot.png", "--caption", ko, "--json"])
            .unwrap()
            .command
            .unwrap()
        {
            Command::SendFile { caption, json, .. } => {
                assert_eq!(caption.as_deref(), Some(ko));
                assert!(json);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn preserves_flags_missing_values_equals_separator_and_help() {
        // Explicit `--flag=value` and positional `--` still allow flag-looking text
        match Cli::try_parse_from([
            "teams",
            "send",
            "chat:c1",
            "--subject=--dry-run",
            "--",
            "--dry-run",
        ])
        .unwrap()
        .command
        .unwrap()
        {
            Command::Send {
                markdown, subject, ..
            } => {
                assert_eq!(markdown, "--dry-run");
                assert_eq!(subject.as_deref(), Some("--dry-run"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match Cli::try_parse_from([
            "teams",
            "edit",
            "chat:c1",
            "m1",
            "--find=--json",
            "--replace=--help",
        ])
        .unwrap()
        .command
        .unwrap()
        {
            Command::Edit { find, replace, .. } => {
                assert_eq!(find.as_deref(), Some("--json"));
                assert_eq!(replace.as_deref(), Some("--help"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        match Cli::try_parse_from(["teams", "search", "--", "--json"])
            .unwrap()
            .command
            .unwrap()
        {
            Command::Search { query, json, .. } => {
                assert_eq!(query, "--json");
                assert!(!json);
            }
            other => panic!("unexpected: {other:?}"),
        }
        match Cli::try_parse_from(["teams", "send", "chat:c1", "---\nbody"])
            .unwrap()
            .command
            .unwrap()
        {
            Command::Send { markdown, .. } => assert_eq!(markdown, "---\nbody"),
            other => panic!("unexpected: {other:?}"),
        }

        // Known and unknown flags must NOT be swallowed as values or positionals
        for invalid in [
            vec!["teams", "send", "chat:c1", "--json"],
            vec!["teams", "send", "chat:c1", "--dry-run"],
            vec!["teams", "send", "chat:c1", "body", "--subject", "--json"],
            vec!["teams", "send", "chat:c1", "body", "--subject", "--dry-run"],
            vec!["teams", "send", "chat:c1", "- 목록 항목", "--subject"],
            vec!["teams", "reply", "chat:c1", "m1", "--json"],
            vec!["teams", "reply", "chat:c1", "m1", "--dry-run"],
            vec!["teams", "edit", "chat:c1", "m1", "--json"],
            vec!["teams", "edit", "chat:c1", "m1", "--dry-run"],
            vec!["teams", "edit", "chat:c1", "m1", "--find", "--replace", "new"],
            vec!["teams", "edit", "chat:c1", "m1", "--find", "old", "--replace", "--json"],
            vec!["teams", "edit", "chat:c1", "m1", "--find", "old", "--replace", "--dry-run"],
            vec!["teams", "edit", "chat:c1", "m1", "--find", "- old", "--replace"],
            vec!["teams", "edit", "chat:c1", "m1", "--subject", "--json"],
            vec!["teams", "update-chat", "chat:c1", "--topic", "--json"],
            vec!["teams", "new-chat", "--to", "a@b", "--message", "--json"],
            vec!["teams", "new-chat", "--to", "a@b", "--topic", "--message", "hi"],
            vec!["teams", "search", "--json"],
            vec!["teams", "search", "--dry-run"],
            // Non-text fields (IDs/targets/numbers) keep their existing parsers
            vec!["teams", "send", "-bad-target", "body"],
            vec!["teams", "search", "- 목록 항목", "--limit", "-5"],
        ] {
            let err = Cli::try_parse_from(invalid.clone()).expect_err(&format!(
                "expected parse error for {invalid:?}"
            ));
            assert!(
                !matches!(
                    err.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ),
                "unexpected help/version for {invalid:?}: {err}"
            );
        }

        // Actual help invocations must return DisplayHelp (never swallowed as text)
        for help_argv in [
            vec!["teams", "send", "--help"],
            vec!["teams", "send", "chat:c1", "--help"],
            vec!["teams", "send", "chat:c1", "-h"],
            vec!["teams", "send", "chat:c1", "- 목록 항목", "--help"],
            vec!["teams", "edit", "chat:c1", "m1", "--help"],
            vec!["teams", "search", "--help"],
            vec!["teams", "search", "-h"],
            vec!["teams", "search", "- 목록 항목", "--help"],
        ] {
            let err = Cli::try_parse_from(help_argv.clone()).expect_err(&format!(
                "expected DisplayHelp for {help_argv:?}"
            ));
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp,
                "expected DisplayHelp for {help_argv:?}, got {err}"
            );
            assert!(
                try_fast_help_or_version_from(help_argv.clone(), Persona::Teams).is_some(),
                "expected fast help for {help_argv:?}"
            );
        }

        // `--subject=--help` and `-- --help` are NOT help invocations
        assert!(
            try_fast_help_or_version_from(
                ["teams", "send", "chat:c1", "body", "--subject=--help"],
                Persona::Teams,
            )
            .is_none()
        );
        assert!(
            try_fast_help_or_version_from(
                ["teams", "send", "chat:c1", "--", "--help"],
                Persona::Teams,
            )
            .is_none()
        );

        // Ordinary argv with no candidate hyphen value must skip clap command inspection
        let to_os = |slice: &[&str]| -> Vec<std::ffi::OsString> {
            slice.iter().map(std::ffi::OsString::from).collect()
        };
        for ordinary in [
            &["teams"][..],
            &["teams", "--help"][..],
            &["teams", "chats", "--limit", "50", "--json"][..],
            &["teams", "read", "chat:c1", "--json", "--compact"][..],
            &["teams", "send", "chat:c1", "hello", "--subject", "notice", "--json"][..],
            &["teams", "send", "chat:c1", "--help"][..],
            &["teams", "send", "chat:c1", "-h"][..],
            &["teams", "send", "chat:c1", "--dry-run"][..],
            &["teams", "send", "chat:c1", "body", "--subject=--dry-run"][..],
            &["teams", "send", "chat:c1", "--", "- 목록 항목"][..],
            &["teams", "edit", "chat:c1", "m1", "--find=--json", "--replace=--help"][..],
            &["teams", "search", "normal query", "--top", "10", "--json"][..],
        ] {
            assert!(
                !has_candidate_hyphen_value(&to_os(ordinary)),
                "expected fast-path skip for {ordinary:?}"
            );
        }
        for candidate in [
            &["teams", "send", "chat:c1", "- 목록 항목"][..],
            &["teams", "msg", "chat:c1", "- first\n- second", "--json"][..],
            &["teams", "edit", "chat:c1", "m1", "--find", "- old", "--replace", "new"][..],
            &["teams", "update-chat", "chat:c1", "--topic", "- 목록 항목"][..],
            &["teams", "search", "- 목록 항목"][..],
        ] {
            assert!(
                has_candidate_hyphen_value(&to_os(candidate)),
                "expected candidate detection for {candidate:?}"
            );
        }
    }

    #[test]
    fn paginate_boundaries_and_cursor_errors() {
        let items: Vec<usize> = (0..120).collect();

        let p1 = paginate(items.clone(), 50, None, 7).unwrap();
        assert_eq!(p1.items.len(), 50);
        assert_eq!(p1.items[0], 0);
        assert!(p1.has_more);
        assert_eq!(p1.next_cursor.as_deref(), Some("50-7"));
        assert_eq!(p1.total, 120);

        let p2 = paginate(items.clone(), 50, Some("50-7"), 7).unwrap();
        assert_eq!(p2.items.len(), 50);
        assert_eq!(p2.items[0], 50);
        assert!(p2.has_more);
        assert_eq!(p2.next_cursor.as_deref(), Some("100-7"));

        let p3 = paginate(items.clone(), 50, Some("100-7"), 7).unwrap();
        assert_eq!(p3.items.len(), 20);
        assert_eq!(p3.items[0], 100);
        assert!(!p3.has_more);
        assert_eq!(p3.next_cursor, None);

        let p_end = paginate(items.clone(), 50, Some("120-7"), 7).unwrap();
        assert!(p_end.items.is_empty());
        assert!(!p_end.has_more);
        assert_eq!(p_end.next_cursor, None);

        let p_all = paginate(items.clone(), 120, None, 7).unwrap();
        assert_eq!(p_all.items.len(), 120);
        assert!(!p_all.has_more);
        assert_eq!(p_all.next_cursor, None);

        let p_empty = paginate(Vec::<usize>::new(), 50, None, 7).unwrap();
        assert!(p_empty.items.is_empty());
        assert!(!p_empty.has_more);
        assert_eq!(p_empty.next_cursor, None);
        assert_eq!(p_empty.total, 0);

        for bad in ["abc", "50", "121-7", "-1-7"] {
            let err = paginate(items.clone(), 50, Some(bad), 7)
                .err()
                .expect("expected invalid cursor error");
            assert!(
                err.to_string().contains("Invalid cursor"),
                "cursor={bad} err={err}"
            );
        }

        let stale = paginate(items, 50, Some("50-8"), 7)
            .err()
            .expect("expected stale cursor error");
        assert!(stale.to_string().contains("Stale cursor"), "{stale}");
    }

    #[test]
    fn compact_json_reports_omissions_and_hint() {
        use crate::models::ReactionSummary;

        let conv = ConversationSummary {
            conv_key: "chat:c1".to_string(),
            target: ConversationTarget::Chat {
                chat_id: "c1".to_string(),
            },
            kind: ConversationKind::Group,
            team_id: None,
            team_name: None,
            title: "Release Room".to_string(),
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

        let base_msg = |id: &str| MessageItem {
            msg_key: format!("chat:c1|{id}"),
            conv_key: "chat:c1".to_string(),
            msg_id: id.to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: Some("u1".to_string()),
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: "2026-11-20T08:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: "<p>hi</p>".to_string(),
            body_plain: "hi".to_string(),
            attachments: vec![],
            mentions: vec![],
            reactions: vec![],
            reply_count: 0,
            web_url: None,
        };

        let m1 = base_msg("m1");
        let mut m2 = base_msg("m2");
        m2.deleted = true;
        m2.body_plain = "(deleted message)".to_string();

        let mut m3 = base_msg("m3");
        m3.subject = Some("Q4 Plan".to_string());
        m3.importance = "high".to_string();
        m3.mentions = vec![
            MentionInfo {
                id: 0,
                user_id: "u1".to_string(),
                display_name: "Alice".to_string(),
            },
            MentionInfo {
                id: 1,
                user_id: "u2".to_string(),
                display_name: "Bob".to_string(),
            },
        ];
        m3.reactions = vec![ReactionSummary {
            reaction_type: "like".to_string(),
            count: 2,
            me_reacted: false,
            users: vec!["Alice".to_string(), "Bob".to_string()],
        }];

        let mut m4 = base_msg("m4");
        m4.reply_to_id = Some("m1".to_string());
        m4.attachments = vec![AttachmentInfo {
            id: "m1".to_string(),
            content_type: "messageReference".to_string(),
            content_url: None,
            name: None,
            content_json: Some("{}".to_string()),
            thumbnail_url: None,
        }];

        let mut m5 = base_msg("m5");
        m5.body_html =
            "<p>Check <a href=\"https://docs.example.com/runbook-42\">Incident Runbook</a></p>"
                .to_string();
        m5.body_plain = "Check Incident Runbook".to_string();

        let mut m6 = base_msg("m6");
        m6.body_html =
            "<p>See <a href=\"https://docs.example.com/bare\">https://docs.example.com/bare</a></p>"
                .to_string();
        m6.body_plain = "See https://docs.example.com/bare".to_string();

        let mut m7 = base_msg("m7");
        m7.attachments = vec![
            AttachmentInfo {
                id: "card-1".to_string(),
                content_type: "application/vnd.microsoft.card.adaptive".to_string(),
                content_url: None,
                name: None,
                content_json: Some(r#"{"type":"AdaptiveCard"}"#.to_string()),
                thumbnail_url: None,
            },
            AttachmentInfo {
                id: "file-1".to_string(),
                content_type: "reference".to_string(),
                content_url: Some("https://contoso.sharepoint.com/report.pdf".to_string()),
                name: Some("report.pdf".to_string()),
                content_json: None,
                thumbnail_url: None,
            },
            AttachmentInfo {
                id: "empty-ct".to_string(),
                content_type: "   ".to_string(),
                content_url: None,
                name: None,
                content_json: None,
                thumbnail_url: None,
            },
        ];

        let res = ReadMessagesResult {
            messages: vec![m1.clone(), m2, m3, m4, m5, m6, m7.clone()],
            has_more: false,
            next_before: Some("m1".to_string()),
        };

        let compact = read_messages_json(&conv, &res, Some("H"));
        assert_eq!(compact["omitted"]["mentions"], 2);
        assert_eq!(compact["omitted"]["reactions"], 1);
        assert_eq!(compact["omitted"]["fields"].as_array().unwrap().len(), 24);
        assert!(
            !compact["omitted"]["fields"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "attachments.content_type")
        );
        assert_eq!(compact["hint"], "H");
        let msgs = compact["messages"].as_array().unwrap();
        assert!(msgs[0].get("deleted").is_none());
        assert!(msgs[0].get("subject").is_none());
        assert!(msgs[0].get("omitted").is_none());
        assert!(msgs[0].get("links").is_none());
        assert_eq!(msgs[1]["deleted"], true);
        assert_eq!(msgs[2]["subject"], "Q4 Plan");
        assert_eq!(msgs[2]["importance"], "high");
        assert_eq!(msgs[2]["omitted"], json!({ "mentions": 2, "reactions": 1 }));
        assert!(msgs[3].get("attachments").is_none());
        assert_eq!(
            msgs[4]["links"],
            json!([{ "text": "Incident Runbook", "url": "https://docs.example.com/runbook-42" }])
        );
        assert!(msgs[5].get("links").is_none(), "bare URL already in body_plain must not add links");
        assert_eq!(
            msgs[6]["attachments"],
            json!([
                {
                    "id": "card-1",
                    "name": null,
                    "content_type": "application/vnd.microsoft.card.adaptive"
                },
                {
                    "id": "file-1",
                    "name": "report.pdf",
                    "content_type": "reference"
                },
                {
                    "id": "empty-ct",
                    "name": null
                }
            ])
        );
        let ack = sent_ack_json(&conv, &m7);
        assert_eq!(ack["message"]["attachments"], msgs[6]["attachments"]);

        let full = read_messages_json(&conv, &res, None);
        assert!(full.get("omitted").is_none());
        assert!(full.get("hint").is_none());

        let thread = ThreadView {
            conv_key: conv.conv_key.clone(),
            root: m1,
            replies: vec![],
        };
        let t_compact = thread_json(
            &conv,
            &thread,
            ThreadCoverage {
                has_more: true,
                reply_total: None,
            },
            Some("H"),
        );
        assert!(t_compact["thread"]["root"]["reply_count"].is_null());
        assert_eq!(t_compact["thread"]["returned"], 0);
        assert_eq!(t_compact["thread"]["has_more"], true);
    }

    #[test]
    fn members_page_distinguishes_empty_unknown_and_legacy() {
        let storage = Storage::open_in_memory().unwrap();
        let mk_conv = |id: &str| ConversationSummary {
            conv_key: format!("chat:{id}"),
            target: ConversationTarget::Chat {
                chat_id: id.to_string(),
            },
            kind: ConversationKind::Group,
            team_id: None,
            team_name: None,
            title: id.to_string(),
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
        let mk_member = |uid: &str, name: &str| MemberInfo {
            user_id: uid.to_string(),
            member_id: None,
            display_name: name.to_string(),
            email: None,
            roles: vec![],
            presence: None,
        };

        // (a) Marker only, 0 rows -> verified empty
        let conv_a = mk_conv("a");
        record_cli_sync_cursor(&storage, &format!("cli:members:{}", conv_a.conv_key));
        let page_a = members_page(&storage, &conv_a, 50, None).unwrap();
        assert_eq!(page_a["total"], 0);
        assert_eq!(page_a["has_more"], false);
        assert!(page_a["fetched_at"].is_string());

        // (b) Neither marker nor rows -> unfetched error
        let conv_b = mk_conv("b");
        let err_b = members_page(&storage, &conv_b, 50, None).unwrap_err();
        assert!(
            err_b.to_string().contains("have not been fetched yet"),
            "{err_b}"
        );

        // (c) Legacy cache: 3 rows via upsert_members, no marker
        let conv_c = mk_conv("c");
        storage
            .upsert_members(
                &conv_c.conv_key,
                &[
                    mk_member("u1", "Alice"),
                    mk_member("u2", "Bob"),
                    mk_member("u3", "Carol"),
                ],
            )
            .unwrap();
        let page_c = members_page(&storage, &conv_c, 50, None).unwrap();
        assert!(page_c["total"].is_null());
        assert!(page_c["has_more"].is_null());
        assert!(page_c["fetched_at"].is_null());
        let page_c2 = members_page(&storage, &conv_c, 2, None).unwrap();
        assert_eq!(page_c2["has_more"], true);
        assert_eq!(page_c2["next_cursor"], "2-0");

        // (d) 3 rows + marker
        let conv_d = mk_conv("d");
        storage
            .replace_members(
                &conv_d.conv_key,
                &[
                    mk_member("u1", "Alice"),
                    mk_member("u2", "Bob"),
                    mk_member("u3", "Carol"),
                ],
            )
            .unwrap();
        record_cli_sync_cursor(&storage, &format!("cli:members:{}", conv_d.conv_key));
        let page_d = members_page(&storage, &conv_d, 50, None).unwrap();
        assert_eq!(page_d["total"], 3);
        assert_eq!(page_d["has_more"], false);
        assert!(page_d["fetched_at"].is_string());
    }

    #[test]
    fn error_json_classifies_runtime_errors_and_candidates() {
        let not_signed = error_json(&anyhow::anyhow!(
            "Sign-in required. Run 'teams login' first."
        ));
        assert_eq!(not_signed["ok"], false);
        assert_eq!(not_signed["error"]["code"], "not_signed_in");
        assert!(not_signed["error"].get("http_status").is_none());

        let expired = error_json(&anyhow::Error::new(DetailedError {
            message: "Token expired (exp: 2026-01-01) — refresh the token and retry.".to_string(),
            details: json!({ "expired": true, "status": "expired" }),
        }));
        assert_eq!(expired["error"]["code"], "token_expired");
        assert_eq!(expired["error"]["details"]["expired"], true);

        let forbidden = error_json(&anyhow::anyhow!(
            "Graph API error (POST https://graph.microsoft.com/v1.0/chats/c1/messages): 403 Forbidden"
        ));
        assert_eq!(forbidden["error"]["code"], "permission_denied");
        assert_eq!(forbidden["error"]["http_status"], 403);

        let not_found = error_json(&anyhow::anyhow!(
            "Cannot reply: message missing-1 was not found in this chat: Graph API error (GET https://graph.microsoft.com/v1.0/chats/c1/messages/missing-1): 404 Not Found"
        ));
        assert_eq!(not_found["error"]["code"], "not_found");
        assert_eq!(not_found["error"]["http_status"], 404);

        // TEAMS-19: a "403" inside the URL / message id must not turn a 404 or 500 into
        // permission_denied; only the parsed response status (or a permission keyword) does.
        let id_with_403 = error_json(&anyhow::anyhow!(
            "Graph API error (GET https://graph.microsoft.com/v1.0/chats/c1/messages/1700000403123): 404 Not Found [NotFound] message 1700000403123 does not exist"
        ));
        assert_eq!(id_with_403["error"]["code"], "not_found");
        assert_eq!(id_with_403["error"]["http_status"], 404);
        let server_error = error_json(&anyhow::anyhow!(
            "Graph API error (GET https://graph.microsoft.com/v1.0/chats/19:403abc/messages): 500 Internal Server Error"
        ));
        assert_eq!(server_error["error"]["code"], "error");
        assert_eq!(server_error["error"]["http_status"], 500);
        assert_eq!(
            error_code_for_message(
                "Graph API error (GET https://graph.microsoft.com/v1.0/me/joinedTeams): 403 Forbidden [Forbidden] Missing scope"
            ),
            "permission_denied"
        );
        assert_eq!(
            error_code_for_message("Authorization_RequestDenied: Insufficient privileges to complete the operation."),
            "permission_denied"
        );
        let untrusted = error_json(&anyhow::anyhow!(
            "Graph HTTP request failed (GET https://graph.microsoft.com/v1.0/me). {} (unknown issuer). If your network uses a TLS-inspecting proxy, ...: error sending request",
            crate::http::TLS_UNTRUSTED_MARKER
        ));
        assert_eq!(untrusted["error"]["code"], "tls_untrusted_certificate");
        assert!(untrusted["error"].get("http_status").is_none());

        let target_missing = error_json(&anyhow::anyhow!(
            "Conversation/channel not found: NonExistentRoom"
        ));
        assert_eq!(target_missing["error"]["code"], "target_not_found");

        let generic = error_json(&anyhow::anyhow!(
            "Deleting with --json requires passing --yes."
        ));
        assert_eq!(generic["error"]["code"], "error");
        assert!(generic["error"].get("http_status").is_none());

        let amb = error_json(&anyhow::Error::new(resolver::AmbiguousTarget {
            message: "Multiple targets match title 'General' exactly.".to_string(),
            candidates: vec![
                resolver::TargetCandidate {
                    conv_key: "chan:team-eng:c1".to_string(),
                    title: "General".to_string(),
                    team_name: Some("Engineering".to_string()),
                },
                resolver::TargetCandidate {
                    conv_key: "chan:team-des:c2".to_string(),
                    title: "General".to_string(),
                    team_name: Some("Design".to_string()),
                },
            ],
        }));
        assert_eq!(amb["error"]["code"], "target_ambiguous");
        assert_eq!(amb["error"]["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(amb["error"]["candidates"][0]["team_name"], "Engineering");

        let unv = error_json(&anyhow::Error::new(resolver::UnverifiedTarget {
            message: "Not sent: cannot confirm target 'General' because the teams/channels lookup failed: Graph API error (GET https://graph.microsoft.com/v1.0/me/joinedTeams): 403 Forbidden".to_string(),
            candidates: vec![resolver::TargetCandidate {
                conv_key: "chat:19:group-general-sync@thread.v2".to_string(),
                title: "General Sync".to_string(),
                team_name: None,
            }],
        }));
        assert_eq!(unv["error"]["code"], "permission_denied");
        assert_eq!(unv["error"]["http_status"], 403);
        assert_eq!(
            unv["error"]["candidates"][0]["conv_key"],
            "chat:19:group-general-sync@thread.v2"
        );
        assert!(unv["error"]["candidates"][0].get("team_name").is_none());
    }

    #[tokio::test]
    async fn build_quote_reply_uses_cache_and_targets_single_message_on_miss() {
        use axum::{routing::get, Json, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let item_hits = Arc::new(AtomicUsize::new(0));
        let list_hits = Arc::new(AtomicUsize::new(0));
        let ih = Arc::clone(&item_hits);
        let lh = Arc::clone(&list_hits);

        let router = Router::new()
            .route(
                "/v1.0/chats/c1/messages/m-old",
                get(move || {
                    let ih = Arc::clone(&ih);
                    async move {
                        ih.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "id": "m-old",
                            "messageType": "message",
                            "createdDateTime": "2026-10-01T10:00:00Z",
                            "from": { "user": { "id": "u2", "displayName": "Alice" } },
                            "body": { "contentType": "text", "content": "older message body" }
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/chats/c1/messages",
                get(move || {
                    let lh = Arc::clone(&lh);
                    async move {
                        lh.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tok);
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me");
        let storage = Storage::open_in_memory().unwrap();
        let conv = ConversationSummary {
            conv_key: "chat:c1".into(),
            target: ConversationTarget::Chat {
                chat_id: "c1".into(),
            },
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: "Alice".into(),
            topic: None,
            unread_count: 0,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: None,
            pinned: false,
            peer_user_id: Some("u2".into()),
            web_url: None,
        };

        // First call (cache miss): must issue 1 targeted GET /messages/m-old and 0 list calls.
        let q1 = build_quote_reply(&storage, &graph, &conv, "m-old")
            .await
            .expect("quoted reply should be built via targeted GET");
        assert_eq!(q1.msg_id, "m-old");
        assert_eq!(q1.author_name, "Alice");
        assert_eq!(q1.preview_plain, "older message body");
        assert_eq!(item_hits.load(Ordering::SeqCst), 1);
        assert_eq!(list_hits.load(Ordering::SeqCst), 0);

        // Second call (now cached in SQLite): must make 0 additional Graph calls.
        let q2 = build_quote_reply(&storage, &graph, &conv, "m-old")
            .await
            .expect("cached quote should succeed");
        assert_eq!(q2.msg_id, "m-old");
        assert_eq!(item_hits.load(Ordering::SeqCst), 1);
        assert_eq!(list_hits.load(Ordering::SeqCst), 0);

        server.abort();
    }
}
