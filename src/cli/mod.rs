//! `teams` CLI parser, command dispatcher, and shared domain helpers for CLI/HTTP/MCP.

mod args;
mod auth;
mod conversations;
mod format;
mod history;
mod messages;
mod preview;
mod runtime;
mod sync_export;

#[cfg(test)]
mod tests;

use anyhow::Result;

use crate::{
    auth::LoginOptions,
    config::{AppConfig, Persona},
    serve::{self, ServeOptions},
};

pub use self::args::{Cli, Command, DaemonCommand};
pub(crate) use self::{
    conversations::{
        fetch_presence_after_set, members_page, normalize_presence_availability, refresh_members,
        resolve_post_target, resolve_target, update_group_chat_topic,
    },
    format::{
        clamp_limit, error_json, error_json_from_message, fetch_stamp, finish_thread_value,
        is_forbidden_error, paginate, read_messages_json, sent_ack_json, thread_json,
        users_search_json, wrap_graph_permission_error,
    },
    history::{
        build_quote_reply, download_message_attachment, edit_sent_message, ensure_mentions_used,
        hydrate_message_attachments, image_mime_for_path, normalize_reaction, parse_mention_spec,
        read_conversation_messages, read_conversation_messages_offline, read_thread,
        read_thread_offline, resolve_before_cursor, send_file_or_inline, thread_cache_miss,
        MessageEdit,
    },
    runtime::{chats_cache_satisfies_limit, record_chats_list_cursor, record_cli_sync_cursor},
};

use self::{
    auth::{cmd_doctor, cmd_login, cmd_logout, cmd_tui, cmd_update, cmd_whoami},
    conversations::{
        cmd_channels, cmd_chats, cmd_members, cmd_new_chat, cmd_presence, cmd_search, cmd_teams,
        cmd_update_chat, cmd_users,
    },
    format::{format_full_args, PostOutput, ReadOutput},
    messages::{
        cmd_delete, cmd_download, cmd_edit, cmd_react, cmd_read, cmd_reply, cmd_send,
        cmd_send_file, cmd_thread_with_cache,
    },
    preview::{cmd_preview_local_file, cmd_preview_teams},
    runtime::{dispatch_daemon_command, open_runtime},
    sync_export::{cmd_export, cmd_sync},
};

const DELETE_JSON_REQUIRES_YES: &str = "Deleting with --json requires passing --yes.";

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
                PostOutput { json, full },
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
                PostOutput { json, full },
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
                PostOutput { json, full },
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
