//! CLI argument definitions (`clap`) and hyphen-leading value normalization.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

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
pub(super) fn has_candidate_hyphen_value(raw: &[std::ffi::OsString]) -> bool {
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
pub(super) fn normalize_cli_args(raw: &[std::ffi::OsString]) -> Option<Vec<std::ffi::OsString>> {
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
    pub(super) fn json_output(&self) -> bool {
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

