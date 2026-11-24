//! Model Context Protocol (MCP) server over stdio (newline-delimited JSON-RPC 2.0).
//!
//! # Framing
//!
//! MCP's stdio transport uses **newline-delimited JSON**: exactly one JSON-RPC 2.0
//! object per line. `stdout` carries protocol messages only; all diagnostics go to
//! `stderr`. Notifications (`notifications/*` or requests without `id`) emit no
//! response.

use std::{io::Write, path::PathBuf};

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};

use crate::{
    cli,
    config::AppConfig,
    graph::GraphClient,
    models::{ConversationSummary, MentionInfo},
    parser, resolver,
    storage::Storage,
};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "teams";
const MAX_RPC_LINE_BYTES: usize = 8 * 1024 * 1024;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

enum BoundedLine {
    Eof,
    Line(String),
    TooLong,
}

async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_bytes: usize,
) -> std::io::Result<BoundedLine> {
    let mut buf = Vec::new();
    let mut oversized = false;

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if oversized {
                return Ok(BoundedLine::TooLong);
            }
            if buf.is_empty() {
                return Ok(BoundedLine::Eof);
            }
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(BoundedLine::Line(s));
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            if !oversized {
                if buf.len() + pos > max_bytes {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(&available[..pos]);
                }
            }
            reader.consume(pos + 1);
            if oversized {
                return Ok(BoundedLine::TooLong);
            }
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let s = String::from_utf8(buf)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            return Ok(BoundedLine::Line(s));
        } else {
            if !oversized {
                if buf.len() + available.len() > max_bytes {
                    oversized = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(available);
                }
            }
            let len = available.len();
            reader.consume(len);
        }
    }
}

async fn with_storage<T, F>(storage: &Storage, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T> + Send + 'static,
{
    let storage = storage.clone();
    tokio::task::spawn_blocking(move || f(&storage))
        .await
        .map_err(|err| anyhow::anyhow!("storage task panicked: {err}"))?
}

async fn map_graph_result<T>(graph: &GraphClient, res: Result<T>) -> Result<T, String> {
    match res {
        Ok(val) => Ok(val),
        Err(err) => Err(format_error(
            cli::wrap_graph_permission_error(graph, err).await,
        )),
    }
}

async fn resolve_target_mcp(
    storage: &Storage,
    graph: &GraphClient,
    target: &str,
    offline: bool,
) -> Result<ConversationSummary, String> {
    if offline {
        let target_owned = target.to_string();
        return with_storage(storage, move |s| {
            let cached = s.list_conversations()?;
            resolver::resolve_from_slice(&cached, &target_owned)
        })
        .await
        .map_err(format_error);
    }

    map_graph_result(graph, cli::resolve_target(storage, graph, target, false).await).await
}

/// Run the stdio MCP server until stdin reaches EOF.
pub async fn run(config: AppConfig, graph: GraphClient, storage: Storage) -> Result<()> {
    eprintln!(
        "{SERVER_NAME} MCP server ready (protocol {LATEST_PROTOCOL_VERSION}, user_id={})",
        graph.my_user_id().unwrap_or("unknown")
    );

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = std::io::stdout();

    loop {
        match read_line_bounded(&mut reader, MAX_RPC_LINE_BYTES).await? {
            BoundedLine::Eof => break,
            BoundedLine::TooLong => {
                let err = error_response(
                    Value::Null,
                    INVALID_REQUEST,
                    format!(
                        "invalid request: line exceeds {} MiB limit",
                        MAX_RPC_LINE_BYTES / (1024 * 1024)
                    ),
                );
                stdout.write_all(encode_message(&err).as_bytes())?;
                stdout.flush()?;
            }
            BoundedLine::Line(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                if let Some(response) = handle_rpc_line(&config, &graph, &storage, trimmed).await {
                    stdout.write_all(encode_message(&response).as_bytes())?;
                    stdout.flush()?;
                }
            }
        }
    }

    eprintln!("{SERVER_NAME} MCP server: stdin closed, shutting down");
    Ok(())
}

/// Single-line JSON-RPC handler exposed for deterministic unit & integration testing.
pub async fn handle_rpc_line(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    line: &str,
) -> Option<Value> {
    let message = match parse_message(line) {
        Ok(msg) => msg,
        Err(err_resp) => return Some(err_resp),
    };

    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let has_id = message.get("id").is_some();

    // Notifications (including `notifications/initialized`) never receive a response.
    if method.starts_with("notifications/") || !has_id {
        return None;
    }

    match method {
        "initialize" => Some(success_response(id, initialize_result(&message))),
        "ping" => Some(success_response(id, json!({}))),
        "tools/list" => Some(success_response(id, json!({ "tools": tool_catalog() }))),
        "tools/call" => {
            let name = message
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = message
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            let outcome = match validate_tool_args(name, &arguments) {
                Ok(()) => call_tool(config, graph, storage, name, &arguments).await,
                Err(error) => Err(error),
            };

            Some(success_response(id, tool_result(outcome)))
        }
        _ => Some(error_response(
            id,
            METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )),
    }
}

async fn call_tool(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    name: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let args = Args::new(arguments);

    match name {
        "list_chats" => {
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;

            let (cached, cache_sufficient): (Vec<ConversationSummary>, bool) =
                with_storage(storage, move |s| {
                    let chats: Vec<ConversationSummary> = s
                        .list_conversations()?
                        .into_iter()
                        .filter(|c| !c.kind.is_channel())
                        .collect();
                    let sufficient = cli::chats_cache_satisfies_limit(s, chats.len(), limit);
                    Ok((chats, sufficient))
                })
                .await
                .map_err(format_error)?;

            let mut chats = if refresh || !cache_sufficient {
                let fetched =
                    map_graph_result(graph, graph.list_chats_with_cursor(limit).await).await?;
                let fetched_chats = fetched.chats.clone();
                let null_topic_keys = fetched.explicit_null_topic_keys;
                let next_link = fetched.next_link;
                let eof = fetched.eof;
                let stored: Vec<ConversationSummary> = with_storage(storage, move |s| {
                    s.upsert_conversations_with_null_topics(&fetched_chats, &null_topic_keys)?;
                    cli::record_chats_list_cursor(s, next_link, eof);
                    Ok(s.list_conversations()?
                        .into_iter()
                        .filter(|c| !c.kind.is_channel())
                        .collect())
                })
                .await
                .map_err(format_error)?;
                if stored.is_empty() {
                    fetched.chats
                } else {
                    stored
                }
            } else {
                cached
            };
            chats.truncate(limit);

            Ok(json!({ "chats": chats }))
        }

        "create_chat" => {
            let members = args.required_str_array("members")?;
            let topic = args.optional_str("topic")?;
            let chat =
                map_graph_result(graph, graph.create_chat(&members, topic.as_deref()).await)
                    .await?;
            let chat_clone = chat.clone();
            with_storage(storage, move |s| {
                s.upsert_conversations(std::slice::from_ref(&chat_clone))?;
                Ok(())
            })
            .await
            .map_err(format_error)?;

            Ok(json!({
                "ok": true,
                "conversation": chat,
            }))
        }

        "update_chat" => {
            let target = args.required_str("target")?;
            let topic = args.required_text("topic")?;
            let chat = map_graph_result(
                graph,
                cli::update_group_chat_topic(storage, graph, &target, &topic).await,
            )
            .await?;
            Ok(json!({ "ok": true, "conversation": chat }))
        }

        "list_teams_channels" => {
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            let cursor = args.optional_str("cursor")?;
            if refresh && cursor.is_some() {
                return Err("cursor cannot be combined with refresh".to_string());
            }

            let (teams, channel_errors, stamp) = if cursor.is_some() {
                with_storage(storage, |s| {
                    let teams = s.list_teams()?;
                    let stamp = cli::fetch_stamp(s, "cli:teams").map_or(0, |(st, _)| st);
                    Ok((teams, None, stamp))
                })
                .await
                .map_err(format_error)?
            } else {
                let cached = with_storage(storage, |s| s.list_teams())
                    .await
                    .map_err(format_error)?;
                if refresh || cached.is_empty() {
                    let fetched = map_graph_result(
                        graph,
                        graph.list_joined_teams_with_channels().await,
                    )
                    .await?;
                    let errors = fetched.channel_errors.clone();
                    let (teams, stamp) = with_storage(storage, move |s| {
                        let merged = s.upsert_joined_teams(&fetched)?;
                        cli::record_cli_sync_cursor(s, "cli:teams");
                        let stored = s.list_teams()?;
                        let teams = if stored.is_empty() { merged } else { stored };
                        let stamp = cli::fetch_stamp(s, "cli:teams").map_or(0, |(st, _)| st);
                        Ok((teams, stamp))
                    })
                    .await
                    .map_err(format_error)?;
                    (teams, Some(errors), stamp)
                } else {
                    let stamp = with_storage(storage, |s| {
                        Ok(cli::fetch_stamp(s, "cli:teams").map_or(0, |(st, _)| st))
                    })
                    .await
                    .map_err(format_error)?;
                    (cached, None, stamp)
                }
            };

            let page =
                cli::paginate(teams, limit, cursor.as_deref(), stamp).map_err(format_error)?;
            let returned = page.items.len();
            let mut out = json!({
                "teams": page.items,
                "returned": returned,
                "has_more": page.has_more,
                "next_cursor": page.next_cursor,
                "total": page.total,
            });
            if let Some(errors) = channel_errors {
                out["channel_errors"] = json!(errors);
            }
            Ok(out)
        }

        "get_messages" => {
            let target = args.required_str("target")?;
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            let before = args.optional_str("before")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let conv = resolve_target_mcp(storage, graph, &target, offline).await?;

            let res = if offline {
                let conv_clone = conv.clone();
                let before_clone = before.clone();
                with_storage(storage, move |s| {
                    cli::read_conversation_messages_offline(
                        s,
                        &conv_clone,
                        limit,
                        before_clone.as_deref(),
                    )
                })
                .await
                .map_err(format_error)?
            } else {
                map_graph_result(
                    graph,
                    cli::read_conversation_messages(
                        storage,
                        graph,
                        &conv,
                        limit,
                        before.as_deref(),
                        false,
                        true,
                    )
                    .await,
                )
                .await?
            };

            let hint = compact.then_some(
                "Call get_messages with the same arguments and \"compact\": false for full message objects.",
            );
            Ok(cli::read_messages_json(&conv, &res, hint))
        }

        "get_thread" => {
            let target = args.required_str("target")?;
            let root_msg_id = args.required_str("root_msg_id")?;
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;

            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);
            let conv = resolve_target_mcp(storage, graph, &target, offline).await?;

            let (thread, coverage) = if offline {
                let conv = conv.clone();
                let root_id = root_msg_id.clone();
                with_storage(storage, move |s| cli::read_thread_offline(s, &conv, &root_id, limit))
                    .await
                    .map_err(format_error)?
                    .ok_or_else(|| format_error(cli::thread_cache_miss(&root_msg_id, limit)))?
            } else {
                map_graph_result(
                    graph,
                    cli::read_thread(storage, graph, &conv, &root_msg_id, limit).await,
                )
                .await?
            };

            let hint = compact.then_some(
                "Call get_thread with the same arguments and \"compact\": false for full message objects.",
            );
            Ok(cli::thread_json(&conv, &thread, coverage, hint))
        }

        "send_message" => {
            let target = args.required_str("target")?;
            let markdown = args.required_text("markdown")?;
            let subject = args.optional_str("subject")?;
            let mentions = args.optional_mentions("mentions")?;
            let requested_mentions = mentions.clone();
            let important = args.optional_bool("important")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let conv = map_graph_result(
                graph,
                cli::resolve_post_target(storage, graph, &target).await,
            )
            .await?;

            let mut outgoing = parser::compile_outgoing_markdown(
                &markdown,
                subject,
                mentions,
                vec![],
                vec![],
                None,
            );
            cli::ensure_mentions_used(&requested_mentions, &outgoing.mentions)
                .map_err(format_error)?;
            if important {
                outgoing.importance = Some("high".to_string());
            }

            let mut sent =
                map_graph_result(graph, graph.send_message(&conv.target, &outgoing).await).await?;
            let sent_clone = sent.clone();
            let _ = with_storage(storage, move |s| {
                s.upsert_messages(std::slice::from_ref(&sent_clone))
            })
            .await;
            cli::hydrate_message_attachments(&mut sent);

            if compact {
                Ok(cli::sent_ack_json(&conv, &sent))
            } else {
                Ok(json!({
                    "ok": true,
                    "conversation": conv,
                    "message": sent,
                }))
            }
        }

        "reply_message" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            let markdown = args.required_text("markdown")?;
            let mentions = args.optional_mentions("mentions")?;
            let requested_mentions = mentions.clone();
            let important = args.optional_bool("important")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let conv = resolve_target_mcp(storage, graph, &target, false).await?;
            let quote = cli::build_quote_reply(storage, graph, &conv, &msg_id).await;
            let mut outgoing = parser::compile_outgoing_markdown(
                &markdown,
                None,
                mentions,
                vec![],
                vec![],
                quote,
            );
            cli::ensure_mentions_used(&requested_mentions, &outgoing.mentions)
                .map_err(format_error)?;
            if important {
                outgoing.importance = Some("high".to_string());
            }

            let mut sent = map_graph_result(
                graph,
                graph.reply_message(&conv.target, &msg_id, &outgoing).await,
            )
            .await?;
            let sent_clone = sent.clone();
            let _ = with_storage(storage, move |s| {
                s.upsert_messages(std::slice::from_ref(&sent_clone))
            })
            .await;
            cli::hydrate_message_attachments(&mut sent);

            if compact {
                let mut ack = cli::sent_ack_json(&conv, &sent);
                ack["reply_to"] = json!(msg_id);
                Ok(ack)
            } else {
                Ok(json!({
                    "ok": true,
                    "conversation": conv,
                    "reply_to": msg_id,
                    "message": sent,
                }))
            }
        }

        "send_file" => {
            let target = args.required_str("target")?;
            let path = args.required_str("path")?;
            let caption = args.optional_str("caption")?;
            let thread_root_id = args.optional_str("thread_root_id")?;
            let inline = args.optional_bool("inline")?.unwrap_or(false);
            let compact = args.optional_bool("compact")?.unwrap_or(true);

            let conv = map_graph_result(
                graph,
                cli::resolve_post_target(storage, graph, &target).await,
            )
            .await?;
            let sent = map_graph_result(
                graph,
                cli::send_file_or_inline(
                    graph,
                    storage,
                    &conv,
                    std::path::Path::new(&path),
                    caption.as_deref(),
                    thread_root_id.as_deref(),
                    inline,
                )
                .await,
            )
            .await?;

            if compact {
                let mut ack = cli::sent_ack_json(&conv, &sent);
                ack["path"] = json!(path);
                ack["inline"] = json!(inline);
                Ok(ack)
            } else {
                Ok(json!({
                    "ok": true,
                    "conversation": conv,
                    "path": path,
                    "inline": inline,
                    "message": sent,
                }))
            }
        }

        "download_attachment" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            let index = args.optional_usize("index")?.unwrap_or(0);
            let thread_root_id = args.optional_str("thread_root_id")?;
            let out_dir = args
                .optional_str("out_dir")?
                .map(PathBuf::from)
                .unwrap_or_else(|| config.resolved_download_dir());

            let conv = resolve_target_mcp(storage, graph, &target, false).await?;
            let downloaded = map_graph_result(
                graph,
                cli::download_message_attachment(
                    graph,
                    storage,
                    &conv,
                    &msg_id,
                    index,
                    thread_root_id.as_deref(),
                    &out_dir,
                )
                .await,
            )
            .await?;

            Ok(json!({
                "ok": true,
                "path": downloaded.path,
                "file_name": downloaded.file_name,
                "mime_type": downloaded.mime_type,
                "size_bytes": downloaded.bytes_written,
                "bytes_written": downloaded.bytes_written,
                "is_image": downloaded.is_image,
            }))
        }

        "edit_message" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            let edit = args.message_edit()?;
            let thread_root_id = args.optional_str("thread_root_id")?;

            let conv = resolve_target_mcp(storage, graph, &target, false).await?;
            map_graph_result(
                graph,
                cli::edit_sent_message(storage, graph, &conv, &msg_id, thread_root_id.as_deref(), edit)
                    .await,
            )
            .await?;

            Ok(json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))
        }

        "delete_message" => {
            let target = args.required_str("target")?;
            let msg_id = args.required_str("msg_id")?;
            args.required_true("confirm")?;
            let thread_root_id = args.optional_str("thread_root_id")?;

            let conv = resolve_target_mcp(storage, graph, &target, false).await?;
            map_graph_result(
                graph,
                graph
                    .delete_message(&conv.target, &msg_id, thread_root_id.as_deref())
                    .await,
            )
            .await?;

            let ck = conv.conv_key.clone();
            let mid = msg_id.clone();
            let now = chrono::Utc::now().to_rfc3339();
            let _ = with_storage(storage, move |s| {
                s.apply_local_soft_delete(&ck, &mid, &now).map(|_| ())
            })
            .await;

            Ok(json!({
                "ok": true,
                "conv_key": conv.conv_key,
                "msg_id": msg_id,
            }))
        }

        "search_messages" => {
            let query = args.required_str("query")?;
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            let target_opt = args.optional_str("target")?;
            let query_clone = query.clone();
            let hits = with_storage(storage, move |s| {
                let conv_key_filter = match target_opt {
                    Some(ref t) => {
                        let cached = s.list_conversations()?;
                        Some(resolver::resolve_from_slice(&cached, t)?.conv_key)
                    }
                    None => None,
                };
                s.search_messages(&query_clone, conv_key_filter.as_deref(), limit)
            })
            .await
            .map_err(format_error)?;

            Ok(json!({
                "query": query,
                "hits": hits,
            }))
        }

        "list_members" => {
            let target = args.required_str("target")?;
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            let cursor = args.optional_str("cursor")?;
            let offline = args.optional_bool("offline")?.unwrap_or(false);
            let from_cache = offline || cursor.is_some();
            let conv = resolve_target_mcp(storage, graph, &target, from_cache).await?;
            if !from_cache {
                map_graph_result(graph, cli::refresh_members(storage, graph, &conv).await).await?;
            }
            let conv_clone = conv.clone();
            let cursor_clone = cursor.clone();
            with_storage(storage, move |s| {
                cli::members_page(s, &conv_clone, limit, cursor_clone.as_deref())
            })
            .await
            .map_err(format_error)
        }

        "search_users" => {
            let query = args.required_str("query")?;
            let limit = bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(10),
                1,
                1_000,
            )?;
            let users = map_graph_result(graph, graph.search_users(&query, limit).await).await?;
            Ok(cli::users_search_json(&users))
        }

        "get_presence" => {
            let user_ids = args.optional_str_array("user_ids")?;
            if let Some(ids) = user_ids {
                let presences =
                    map_graph_result(graph, graph.get_presences_by_user_ids(&ids).await).await?;
                Ok(json!({ "presences": presences }))
            } else {
                let presence = map_graph_result(graph, graph.get_my_presence().await).await?;
                Ok(json!({ "presence": presence }))
            }
        }

        other => Err(format!("unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC framing
// ---------------------------------------------------------------------------

fn parse_message(line: &str) -> Result<Value, Value> {
    let trimmed = line.trim();
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(error) => {
            return Err(error_response(
                Value::Null,
                PARSE_ERROR,
                format!("parse error: {error}"),
            ));
        }
    };

    let Some(object) = value.as_object() else {
        return Err(error_response(
            Value::Null,
            INVALID_REQUEST,
            "invalid request: expected a JSON object",
        ));
    };

    let id = object.get("id").cloned().unwrap_or(Value::Null);

    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: jsonrpc must be \"2.0\"",
        ));
    }

    if !object.get("method").is_some_and(Value::is_string) {
        return Err(error_response(
            id,
            INVALID_REQUEST,
            "invalid request: method must be a string",
        ));
    }

    Ok(value)
}

fn encode_message(value: &Value) -> String {
    let body = serde_json::to_string(value).unwrap_or_else(|_| {
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"serialization failed\"}}"
            .to_string()
    });
    format!("{body}\n")
}

fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() },
    })
}

fn initialize_result(message: &Value) -> Value {
    let requested = message
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str);
    let protocol_version = requested
        .filter(|version| SUPPORTED_PROTOCOL_VERSIONS.contains(version))
        .unwrap_or(LATEST_PROTOCOL_VERSION);

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": false }
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Microsoft Teams tools via Microsoft Graph v1.0. Teams targets accept conv_key (chat:... or chan:team:channel), \"Team/Channel\", or a unique conversation title substring. Use get_messages with optional before (ISO-8601 timestamp, YYYY-MM-DD, or msg_id) and limit to paginate older history. Destructive operations require confirm: true."
    })
}

// ---------------------------------------------------------------------------
// Tool catalog (16 Teams tools)
// ---------------------------------------------------------------------------

fn mentions_schema() -> Value {
    json!({
        "type": "array",
        "description": "Users to @-mention, as \"user_id:DisplayName\" strings or {user_id, display_name} objects. Each mentioned user's @DisplayName must appear in markdown or the call fails before sending.",
        "items": {
            "oneOf": [
                { "type": "string" },
                {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer", "minimum": 0, "description": "Optional 0-based mention placeholder index." },
                        "user_id": { "type": "string", "minLength": 1, "description": "Microsoft Entra user ID." },
                        "display_name": { "type": "string", "minLength": 1, "description": "Display name matching @DisplayName in markdown." }
                    },
                    "required": ["user_id", "display_name"],
                    "additionalProperties": false
                }
            ]
        }
    })
}

fn tool_catalog() -> Vec<Value> {
    vec![
        json!({
            "name": "list_chats",
            "description": "List 1:1, group, and meeting chats (use list_teams_channels for team channels). Reads the local SQLite cache with no Graph request when the cache covers limit; set refresh=true to sync live from Microsoft Graph (about 1 request per 50 chats). Requires delegated Chat.Read. Returns { chats }.",
            "annotations": {
                "title": "List Chats",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "default": false, "description": "Sync chats live from Microsoft Graph before returning (about 1 request per 50 chats)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of chats to return (1..1000, default 50)." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "create_chat",
            "description": "Create a 1:1 chat (1 member; reuses an existing 1:1 chat if one exists) or a new group chat (2+ members). Use search_users first if you need peer Entra user IDs or email addresses. Requires delegated Chat.Create or Chat.ReadWrite. Writes to Microsoft Graph and caches the new conversation; returns { ok, conversation }.",
            "annotations": {
                "title": "Create Chat",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "members": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Peer Microsoft Entra user IDs or email addresses (1 item for a 1:1 chat, 2+ items for a group chat; the signed-in user is included automatically)."
                    },
                    "topic": { "type": "string", "description": "Optional topic title applied only when creating a group chat (2+ members)." }
                },
                "required": ["members"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "update_chat",
            "description": "Rename an existing group chat's topic (not supported on 1:1 chats or team channels). Requires delegated Chat.ReadWrite and membership in the group chat. Updates Microsoft Graph and the local cache; returns { ok, conversation }.",
            "annotations": {
                "title": "Update Group Chat Topic",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Group chat conv_key (chat:...), raw chat ID, or unique chat title substring." },
                    "topic": { "type": "string", "minLength": 1, "maxLength": 250, "pattern": "^[^:]+$", "description": "New group chat topic (1..250 characters; colons are disallowed)." }
                },
                "required": ["target", "topic"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_teams_channels",
            "description": "List joined Microsoft Teams and their channels (use list_chats for 1:1/group chats). Cached reads and cursor pages make no Graph request; refresh=true or an empty cache calls Graph for teams (1+ requests) then channels (1 $batch per 20 teams). Requires Team.ReadBasic.All and Channel.ReadBasic.All. Returns paginated { teams, returned, has_more, next_cursor, total }.",
            "annotations": {
                "title": "List Teams and Channels",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "default": false, "description": "Sync teams and channels live from Microsoft Graph before returning (1+ requests for teams, plus 1 $batch per 20 teams; cannot combine with cursor)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of teams to return per page (1..1000, default 50)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous page; reads the next page from the local SQLite cache without calling Microsoft Graph." }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_messages",
            "description": "Read messages from a chat or team channel, oldest first (use get_thread to read replies under a channel root post, or search_messages for full-text search across cached history). Fetches live from Microsoft Graph by default (about 1 request per 50 messages); set offline=true to read only the local SQLite cache. Returns { conversation, messages, has_more, next_before }.",
            "annotations": {
                "title": "Get Conversation Messages",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Conversation conv_key (chat:... or chan:team:channel), \"Team/Channel\", or unique conversation title substring." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of messages to return (1..1000, default 50)." },
                    "before": { "type": "string", "description": "Optional ISO-8601 timestamp, YYYY-MM-DD date, or cached msg_id to paginate older messages before that point." },
                    "offline": { "type": "boolean", "default": false, "description": "When true, read only from the local SQLite cache without calling Microsoft Graph." },
                    "compact": { "type": "boolean", "default": true, "description": "Compact output (default true) omitting verbose metadata; set false for full conversation and message objects." }
                },
                "required": ["target"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_thread",
            "description": "Read a team channel root post and up to limit replies, oldest first (use get_messages for top-level channel posts or 1:1/group chats). Online by default (1+ Graph requests, 50 replies each); offline=true reads verified cached replies with no Graph call and errors on cache miss. Returns { conversation, thread: { root, replies, returned, has_more } }.",
            "annotations": {
                "title": "Get Channel Thread",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Channel conv_key (chan:team:channel), \"Team/Channel\", or unique channel title substring." },
                    "root_msg_id": { "type": "string", "minLength": 1, "description": "Message ID of the channel root post whose replies should be fetched." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of replies to return, excluding the root post (1..1000, default 50)." },
                    "offline": { "type": "boolean", "default": false, "description": "Read verified replies from the local SQLite cache with no Graph request; returns an error if cached coverage is missing or incomplete." },
                    "compact": { "type": "boolean", "default": true, "description": "Compact output (default true); set false for full conversation and message objects." }
                },
                "required": ["target", "root_msg_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "send_message",
            "description": "Post a new Markdown message to a chat or a new root post to a team channel (use reply_message to reply inside a channel thread or quote-reply an existing chat message). Irreversible external side effect notifying conversation members. Requires ChatMessage.Send or ChannelMessage.Send. Returns compact acknowledgement { ok, conversation, message } by default.",
            "annotations": {
                "title": "Send Message",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Conversation conv_key (chat:... or chan:team:channel), \"Team/Channel\", or unique conversation title substring." },
                    "markdown": { "type": "string", "minLength": 1, "description": "Non-empty Markdown message body to compile to Teams HTML and send." },
                    "subject": { "type": "string", "description": "Optional plain-text subject line (applies to team channel root posts)." },
                    "mentions": mentions_schema(),
                    "important": { "type": "boolean", "default": false, "description": "When true, marks the sent message with high importance." },
                    "compact": { "type": "boolean", "default": true, "description": "Return a compact receipt (default true); set false for full conversation and message objects." }
                },
                "required": ["target", "markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "reply_message",
            "description": "Reply to a team channel root post in its thread, or quote-reply a specific message in a 1:1/group chat (use send_message to start a new channel thread or unquoted chat post). Irreversible external side effect notifying participants. Requires ChatMessage.Send or ChannelMessage.Send. Returns { ok, conversation, reply_to, message }.",
            "annotations": {
                "title": "Reply to Message",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Conversation conv_key (chat:... or chan:team:channel), \"Team/Channel\", or unique conversation title substring." },
                    "msg_id": { "type": "string", "minLength": 1, "description": "Channel thread root post ID to reply under, or chat message ID to quote-reply." },
                    "markdown": { "type": "string", "minLength": 1, "description": "Non-empty Markdown reply body." },
                    "mentions": mentions_schema(),
                    "important": { "type": "boolean", "default": false, "description": "When true, marks the reply with high importance." },
                    "compact": { "type": "boolean", "default": true, "description": "Return a compact receipt (default true); set false for full conversation and message objects." }
                },
                "required": ["target", "msg_id", "markdown"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "send_file",
            "description": "Upload a local file from the host running teams to OneDrive/SharePoint and post it as a Teams message attachment (or inline image when inline=true). Irreversible external side effect. Latency scales with file size (single upload up to 4 MiB, 3.2 MiB chunks above). Requires Files.ReadWrite and message send permissions. Returns { ok, conversation, path, inline, message }.",
            "annotations": {
                "title": "Send File or Inline Image",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Conversation conv_key (chat:... or chan:team:channel), \"Team/Channel\", or unique conversation title substring." },
                    "path": { "type": "string", "minLength": 1, "description": "Readable local file path on the host running teams mcp." },
                    "caption": { "type": "string", "description": "Optional Markdown caption posted alongside the file or inline image." },
                    "thread_root_id": { "type": "string", "description": "Optional channel thread root post ID when attaching to an existing channel thread." },
                    "inline": { "type": "boolean", "default": false, "description": "When true, embeds a supported image directly as Teams hostedContents instead of a OneDrive/SharePoint file attachment." },
                    "compact": { "type": "boolean", "default": true, "description": "Return a compact receipt (default true); set false for full conversation and message objects." }
                },
                "required": ["target", "path"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "download_attachment",
            "description": "Download a file attachment or inline Teams screenshot (hostedContents) from a message to local disk so an agent or user can inspect it. Writes a file to out_dir (or configured downloads folder); latency scales with attachment size. Returns { ok, path (absolute), file_name, mime_type, size_bytes, bytes_written, is_image }.",
            "annotations": {
                "title": "Download Attachment",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Conversation conv_key (chat:... or chan:team:channel), \"Team/Channel\", or unique conversation title substring." },
                    "msg_id": { "type": "string", "minLength": 1, "description": "Message ID containing the attachment or inline image." },
                    "index": { "type": "integer", "minimum": 0, "default": 0, "description": "0-based index into the message's attachments/hosted images (default 0)." },
                    "out_dir": { "type": "string", "description": "Optional local directory to save the downloaded file (defaults to configured downloads folder)." },
                    "thread_root_id": { "type": "string", "description": "Optional channel thread root post ID required when msg_id is a reply inside a channel thread." }
                },
                "required": ["target", "msg_id"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "edit_message",
            "description": "Edit a message previously sent by the signed-in user. Choose exactly one mode: markdown (replace full body), find + replace (replace one unique literal phrase while preserving HTML), or subject (rename a channel root post title). Requires Chat.ReadWrite (chats) or ChannelMessage.ReadWrite (channels) and Teams policy allowing edits. Returns { ok, conv_key, msg_id }.",
            "annotations": {
                "title": "Edit Sent Message",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Conversation conv_key (chat:... or chan:team:channel), \"Team/Channel\", or unique conversation title substring." },
                    "msg_id": { "type": "string", "minLength": 1, "description": "ID of the message sent by the current user to edit." },
                    "markdown": { "type": "string", "minLength": 1, "description": "Markdown to replace the entire message body (mutually exclusive with find/replace and subject)." },
                    "find": { "type": "string", "minLength": 1, "description": "Exact phrase occurring once in a single editable text node (must be paired with replace)." },
                    "replace": { "type": "string", "description": "Literal replacement text for find; pass an empty string to remove the matched phrase." },
                    "subject": { "type": "string", "minLength": 1, "description": "New plain-text title for a channel root post you sent (mutually exclusive with body edits and thread_root_id)." },
                    "thread_root_id": { "type": "string", "description": "Optional channel thread root post ID required when editing a reply inside a channel thread." }
                },
                "required": ["target", "msg_id"],
                "oneOf": [
                    { "required": ["markdown"], "not": { "anyOf": [{ "required": ["find"] }, { "required": ["replace"] }, { "required": ["subject"] }] } },
                    { "required": ["find", "replace"], "not": { "anyOf": [{ "required": ["markdown"] }, { "required": ["subject"] }] } },
                    { "required": ["subject"], "not": { "anyOf": [{ "required": ["markdown"] }, { "required": ["find"] }, { "required": ["replace"] }, { "required": ["thread_root_id"] }] } }
                ],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "delete_message",
            "description": "Soft-delete a message previously sent by the signed-in user (use edit_message instead to fix a typo). Destructive action requiring literal confirm=true; rejected during validation otherwise. Enforces Graph delegated permissions (Chat.ReadWrite or ChannelMessage.ReadWrite) and Teams delete policy. Updates Graph and local cache; returns { ok, conv_key, msg_id }.",
            "annotations": {
                "title": "Delete Sent Message",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Conversation conv_key (chat:... or chan:team:channel), \"Team/Channel\", or unique conversation title substring." },
                    "msg_id": { "type": "string", "minLength": 1, "description": "ID of the message sent by the current user to soft-delete." },
                    "confirm": { "const": true, "description": "Safety guard: must be literal JSON boolean true or the call fails before contacting Microsoft Graph." },
                    "thread_root_id": { "type": "string", "description": "Optional channel thread root post ID required when deleting a reply inside a channel thread." }
                },
                "required": ["target", "msg_id", "confirm"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_messages",
            "description": "Full-text search cached messages locally using SQLite FTS5 (no Microsoft Graph request). Only finds messages already fetched into cache.db; call get_messages first if recent history is not cached yet. Returns { query, hits }.",
            "annotations": {
                "title": "Search Cached Messages",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "description": "Full-text search query (matches cached message body, subject, and sender)." },
                    "target": { "type": "string", "description": "Optional cached conversation conv_key, \"Team/Channel\", or unique title substring to scope the search." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of search hits to return (1..1000, default 50)." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "list_members",
            "description": "List members of a chat or channel with Entra user_id and display_name for @-mentions or get_presence (use search_users for directory lookup). Calls Graph (1+ requests) unless cursor or offline=true reads the local SQLite cache only. Returns paginated { conv_key, title, members, returned, has_more, next_cursor, total }.",
            "annotations": {
                "title": "List Conversation Members",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "Conversation conv_key (chat:... or chan:team:channel), \"Team/Channel\", or unique conversation title substring." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 50, "description": "Maximum number of members to return per page (1..1000, default 50)." },
                    "cursor": { "type": "string", "description": "Opaque next_cursor from a previous page; reads the next page from the local SQLite cache without calling Microsoft Graph." },
                    "offline": { "type": "boolean", "default": false, "description": "Read cached members from the local SQLite cache without calling Microsoft Graph (errors if members were never cached)." }
                },
                "required": ["target"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "search_users",
            "description": "Search Microsoft Entra directory users by case-insensitive display name, email, or UPN prefix (use list_members to list participants of a specific chat/channel). Costs 1 Graph request (2 if the tenant rejects $filter and falls back to $search). Requires delegated User.ReadBasic.All or User.Read.All. Returns { users: [{ user_id, display_name, email, user_principal_name }], returned }.",
            "annotations": {
                "title": "Search Directory Users",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "description": "Case-insensitive prefix matched against displayName, mail, and userPrincipalName." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 10, "description": "Maximum number of matching directory users to return (1..1000, default 10)." }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_presence",
            "description": "Fetch current Microsoft Teams availability and activity status for the signed-in user or a batch of Entra user IDs (use search_users or list_members first to look up user IDs). Costs 1 Graph request per 650 user IDs. Requires delegated Presence.Read (self) or Presence.Read.All (others). Returns { presence } when user_ids is omitted, or { presences } when provided.",
            "annotations": {
                "title": "Get User Presence",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": true
            },
            "inputSchema": {
                "type": "object",
                "properties": {
                    "user_ids": {
                        "type": "array",
                        "items": { "type": "string", "minLength": 1 },
                        "description": "Optional list of Microsoft Entra user IDs to query in batch. Omit to fetch only the signed-in user's presence."
                    }
                },
                "additionalProperties": false
            }
        }),
    ]
}

fn tool_result(outcome: Result<Value, String>) -> Value {
    match outcome {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            })
        }
        Err(error) => {
            let payload = cli::error_json_from_message(&error);
            let text = serde_json::to_string(&payload).unwrap_or_else(|_| payload.to_string());
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": true
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

fn validate_schema_additional_properties(name: &str, arguments: &Value) -> Result<(), String> {
    use std::collections::{HashMap, HashSet};
    use std::sync::OnceLock;

    static SCHEMA_PROPERTIES: OnceLock<HashMap<String, (HashSet<String>, bool)>> = OnceLock::new();
    let map = SCHEMA_PROPERTIES.get_or_init(|| {
        let mut out = HashMap::new();
        for tool in tool_catalog() {
            let Some(tool_name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            let schema = &tool["inputSchema"];
            let disallow_additional = schema
                .get("additionalProperties")
                .and_then(Value::as_bool)
                == Some(false);
            let props: HashSet<String> = schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|obj| obj.keys().cloned().collect())
                .unwrap_or_default();
            out.insert(tool_name.to_string(), (props, disallow_additional));
        }
        out
    });

    let Some((allowed_props, disallow_additional)) = map.get(name) else {
        return Err(format!("unknown tool: {name}"));
    };
    let obj = Args::new(arguments).object()?;
    if *disallow_additional {
        if let Some(key) = obj.keys().find(|k| !allowed_props.contains(k.as_str())) {
            return Err(format!("unsupported argument: {key}"));
        }
    }
    Ok(())
}

fn validate_tool_args(name: &str, arguments: &Value) -> Result<(), String> {
    validate_schema_additional_properties(name, arguments)?;
    let args = Args::new(arguments);

    match name {
        "list_chats" => {
            args.optional_bool("refresh")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
        }
        "create_chat" => {
            args.required_str_array("members")?;
            args.optional_str("topic")?;
        }
        "update_chat" => {
            args.reject_unknown(&["target", "topic"])?;
            args.required_str("target")?;
            args.required_text("topic")?;
        }
        "list_teams_channels" => {
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            let cursor = args.optional_str("cursor")?;
            if refresh && cursor.is_some() {
                return Err("cursor cannot be combined with refresh".to_string());
            }
        }
        "get_messages" => {
            args.reject_unknown(&["target", "limit", "before", "offline", "compact"])?;
            args.required_str("target")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            args.optional_str("before")?;
            args.optional_bool("offline")?;
            args.optional_bool("compact")?;
        }
        "get_thread" => {
            args.reject_unknown(&["target", "root_msg_id", "limit", "offline", "compact"])?;
            args.required_str("target")?;
            args.required_str("root_msg_id")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            args.optional_bool("offline")?;
            args.optional_bool("compact")?;
        }
        "send_message" => {
            args.reject_unknown(&[
                "target", "markdown", "subject", "mentions", "important", "compact",
            ])?;
            args.required_str("target")?;
            args.required_text("markdown")?;
            args.optional_str("subject")?;
            args.optional_mentions("mentions")?;
            args.optional_bool("important")?;
            args.optional_bool("compact")?;
        }
        "reply_message" => {
            args.reject_unknown(&[
                "target", "msg_id", "markdown", "mentions", "important", "compact",
            ])?;
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.required_text("markdown")?;
            args.optional_mentions("mentions")?;
            args.optional_bool("important")?;
            args.optional_bool("compact")?;
        }
        "send_file" => {
            args.reject_unknown(&[
                "target", "path", "caption", "thread_root_id", "inline", "compact",
            ])?;
            args.required_str("target")?;
            args.required_str("path")?;
            args.optional_str("caption")?;
            args.optional_str("thread_root_id")?;
            args.optional_bool("inline")?;
            args.optional_bool("compact")?;
        }
        "download_attachment" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.optional_usize("index")?;
            args.optional_str("out_dir")?;
            args.optional_str("thread_root_id")?;
        }
        "edit_message" => {
            args.reject_unknown(&[
                "target", "msg_id", "markdown", "find", "replace", "subject", "thread_root_id",
            ])?;
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.message_edit()?;
            args.optional_str("thread_root_id")?;
        }
        "delete_message" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.required_true("confirm")?;
            args.optional_str("thread_root_id")?;
        }
        "search_messages" => {
            args.required_str("query")?;
            args.optional_str("target")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
        }
        "list_members" => {
            args.required_str("target")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            args.optional_str("cursor")?;
            args.optional_bool("offline")?;
        }
        "search_users" => {
            args.reject_unknown(&["query", "limit"])?;
            args.required_str("query")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(10),
                1,
                1_000,
            )?;
        }
        "get_presence" => {
            args.optional_str_array("user_ids")?;
        }
        other => return Err(format!("unknown tool: {other}")),
    }

    Ok(())
}

fn bounds(name: &str, value: usize, min: usize, max: usize) -> Result<usize, String> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{name} must be between {min} and {max}"))
    }
}

struct Args<'a> {
    value: &'a Value,
}

impl<'a> Args<'a> {
    fn new(value: &'a Value) -> Self {
        Self { value }
    }

    fn object(&self) -> Result<&'a serde_json::Map<String, Value>, String> {
        self.value
            .as_object()
            .ok_or_else(|| "arguments must be a JSON object".to_string())
    }

    fn reject_unknown(&self, allowed: &[&str]) -> Result<(), String> {
        if let Some(key) = self.object()?.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(format!("unsupported argument: {key}"));
        }
        Ok(())
    }

    fn message_edit(&self) -> Result<cli::MessageEdit<'a>, String> {
        let text = |key| match self.object()?.get(key) {
            None => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.as_str())),
            Some(_) => Err(format!("{key} must be a string")),
        };
        let subject = text("subject")?;
        if subject.is_some() && self.object()?.contains_key("thread_root_id") {
            return Err("Subject edits require a channel root post.".to_string());
        }
        cli::MessageEdit::from_args(text("markdown")?, text("find")?, text("replace")?, subject)
            .map_err(format_error)
    }

    fn optional_str(&self, key: &str) -> Result<Option<String>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn required_str(&self, key: &str) -> Result<String, String> {
        self.optional_str(key)?
            .ok_or_else(|| format!("missing required argument: {key}"))
    }

    fn required_text(&self, key: &str) -> Result<String, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Err(format!("missing required argument: {key}")),
            Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
            Some(Value::String(_)) => Err(format!("{key} must not be empty")),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    fn required_true(&self, key: &str) -> Result<(), String> {
        match self.object()?.get(key) {
            Some(Value::Bool(true)) => Ok(()),
            Some(Value::Bool(false)) => Err(format!("{key} must be true")),
            None | Some(Value::Null) => {
                Err(format!("missing required argument: {key} (must be true)"))
            }
            Some(_) => Err(format!("{key} must be a boolean true")),
        }
    }

    fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(format!("{key} must be a boolean")),
        }
    }

    fn optional_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
                .map(Some)
                .ok_or_else(|| format!("{key} must be an integer")),
        }
    }

    fn optional_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.optional_i64(key)? {
            None => Ok(None),
            Some(v) => usize::try_from(v)
                .map(Some)
                .map_err(|_| format!("{key} must be a non-negative integer")),
        }
    }

    fn optional_str_array(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let s = item
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| format!("{key}[{i}] must be a non-empty string"))?;
                    out.push(s.to_string());
                }
                Ok(Some(out))
            }
            Some(_) => Err(format!("{key} must be an array of strings")),
        }
    }

    fn required_str_array(&self, key: &str) -> Result<Vec<String>, String> {
        let items = self
            .optional_str_array(key)?
            .ok_or_else(|| format!("missing required argument: {key}"))?;
        if items.is_empty() {
            return Err(format!("{key} must not be empty"));
        }
        Ok(items)
    }

    fn optional_mentions(&self, key: &str) -> Result<Vec<MentionInfo>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    parse_mention_value(item, idx as i32)
                        .map_err(|e| format!("mentions[{idx}]: {e}"))
                })
                .collect(),
            Some(_) => Err(format!("{key} must be an array")),
        }
    }
}

fn parse_mention_value(value: &Value, default_id: i32) -> Result<MentionInfo, String> {
    if let Some(spec) = value.as_str() {
        return cli::parse_mention_spec(spec, default_id).map_err(format_error);
    }

    let obj = value
        .as_object()
        .ok_or_else(|| "mention must be a string or object".to_string())?;
    let user_id = obj
        .get("user_id")
        .or_else(|| obj.get("userId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "mention.user_id must be a non-empty string".to_string())?;
    let display_name = obj
        .get("display_name")
        .or_else(|| obj.get("displayName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "mention.display_name must be a non-empty string".to_string())?;
    let id = obj
        .get("id")
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(default_id);

    Ok(MentionInfo {
        id,
        user_id: user_id.to_string(),
        display_name: display_name.to_string(),
    })
}

fn format_error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ConversationTarget;

    // Tool descriptions are read by agents before every call: state what a call costs, and keep
    // them short because each one is paid for in every agent context.
    #[test]
    fn tool_descriptions_state_request_cost_and_stay_short() {
        const BUDGET: usize = 400;
        for tool in tool_catalog() {
            let name = tool["name"].as_str().unwrap();
            let description = tool["description"].as_str().unwrap();
            assert!(description.len() <= BUDGET, "{name}: {} chars", description.len());
            let props = &tool["inputSchema"]["properties"];
            if let Some(refresh) = props.get("refresh") {
                let d = refresh["description"].as_str().unwrap_or("");
                assert!(d.contains("Graph") && d.contains("request"), "{name}.refresh: {d}");
            }
            if let Some(offline) = props.get("offline") {
                let d = offline["description"].as_str().unwrap_or("");
                assert!(
                    d.contains("no Graph request") || d.contains("without calling Microsoft Graph"),
                    "{name}.offline: {d}"
                );
            }
        }
        let cost_stated = |name: &str| {
            tool_catalog()
                .into_iter()
                .find(|t| t["name"] == name)
                .and_then(|t| t["description"].as_str().map(str::to_string))
                .is_some_and(|d| d.contains("request") || d.contains("size"))
        };
        for name in [
            "list_chats",
            "list_teams_channels",
            "get_messages",
            "get_thread",
            "list_members",
            "send_file",
            "download_attachment",
            "search_messages",
            "search_users",
            "get_presence",
        ] {
            assert!(cost_stated(name), "{name} has no cost hint");
        }
    }
    use crate::auth::AuthManager;

    fn test_ctx() -> (AppConfig, GraphClient, Storage) {
        let cfg = AppConfig::default();
        let auth = AuthManager::new(cfg.clone()).unwrap();
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        (cfg, graph, storage)
    }

    #[tokio::test]
    async fn handshake_and_notifications_and_ping() {
        let (cfg, graph, storage) = test_ctx();

        // Supported version echoed back
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(resp["result"]["serverInfo"]["name"], "teams");
        assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], false);

        // Unknown protocol version falls back to 2025-06-18
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");

        // Notification produces no response
        assert!(handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )
        .await
        .is_none());

        // Ping returns empty object
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":3,"method":"ping"}"#,
        )
        .await
        .unwrap();
        assert_eq!(resp["result"], json!({}));

        // Malformed JSON -> -32700
        let err = handle_rpc_line(&cfg, &graph, &storage, "{not json")
            .await
            .unwrap();
        assert_eq!(err["error"]["code"], PARSE_ERROR);

        // Unknown method -> -32601
        let err = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":4,"method":"unknown/op"}"#,
        )
        .await
        .unwrap();
        assert_eq!(err["error"]["code"], METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn tools_list_exposes_every_cataloged_tool() {
        let (cfg, graph, storage) = test_ctx();
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            r#"{"jsonrpc":"2.0","id":10,"method":"tools/list"}"#,
        )
        .await
        .unwrap();

        let tools = resp["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(
            names,
            vec![
                "list_chats",
                "create_chat",
                "update_chat",
                "list_teams_channels",
                "get_messages",
                "get_thread",
                "send_message",
                "reply_message",
                "send_file",
                "download_attachment",
                "edit_message",
                "delete_message",
                "search_messages",
                "list_members",
                "search_users",
                "get_presence",
            ]
        );
        for name in &names {
            assert!(
                !name.contains("mail")
                    && !name.contains("calendar")
                    && !name.contains("contact")
                    && !name.contains("todo")
                    && !name.starts_with("outlook_")
                    && !name.starts_with("onenote_"),
                "unexpected non-Teams tool: {name}"
            );
        }

        for tool in tools {
            let tool_name = tool["name"].as_str().unwrap();
            assert_eq!(tool["inputSchema"]["type"], "object", "{tool_name}");
            assert!(tool["description"].as_str().is_some_and(|d| !d.is_empty()), "{tool_name}");
            let ann = tool["annotations"]
                .as_object()
                .unwrap_or_else(|| panic!("missing annotations on {tool_name}"));
            for key in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
                assert!(
                    ann.get(key).is_some_and(Value::is_boolean),
                    "missing boolean annotation {key} on {tool_name}"
                );
            }
            if let Some(props) = tool["inputSchema"]["properties"].as_object() {
                for (prop_name, prop_schema) in props {
                    assert!(
                        prop_schema["description"]
                            .as_str()
                            .is_some_and(|d| !d.is_empty()),
                        "missing description on {tool_name}.{prop_name}"
                    );
                }
            }
        }

        let get_msgs = tools.iter().find(|t| t["name"] == "get_messages").unwrap();
        assert!(get_msgs["inputSchema"]["properties"]["before"].is_object());
    }

    #[tokio::test]
    async fn delete_message_rejects_missing_or_non_literal_true_confirm() {
        let (cfg, graph, storage) = test_ctx();

        for bad_args in [
            json!({ "target": "chat:1", "msg_id": "m1" }),
            json!({ "target": "chat:1", "msg_id": "m1", "confirm": false }),
            json!({ "target": "chat:1", "msg_id": "m1", "confirm": "true" }),
            json!({ "target": "chat:1", "msg_id": "m1", "confirm": 1 }),
            json!({ "target": "chat:1", "msg_id": "m1", "confirm": null }),
        ] {
            let line = json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": {
                    "name": "delete_message",
                    "arguments": bad_args,
                }
            })
            .to_string();
            let resp = handle_rpc_line(&cfg, &graph, &storage, &line).await.unwrap();
            assert_eq!(resp["result"]["isError"], true, "{resp}");
            let msg = resp["result"]["content"][0]["text"].as_str().unwrap();
            assert!(msg.contains("confirm"), "{msg}");
        }

        // Literal true passes validation
        validate_tool_args(
            "delete_message",
            &json!({ "target": "chat:1", "msg_id": "m1", "confirm": true }),
        )
        .unwrap();
    }

    #[test]
    fn argument_validation_covers_tools() {
        assert!(validate_tool_args(
            "get_messages",
            &json!({ "target": "chat:1", "limit": 100, "before": "2026-11-01" })
        )
        .is_ok());
        assert!(validate_tool_args(
            "get_messages",
            &json!({ "target": "chat:1", "before": 123 })
        )
        .is_err());

        assert!(validate_tool_args(
            "send_message",
            &json!({
                "target": "Engineering/General",
                "markdown": "Hello @Alice",
                "mentions": ["u1:Alice", { "user_id": "u2", "display_name": "Bob" }]
            })
        )
        .is_ok());

        assert!(validate_tool_args(
            "send_message",
            &json!({ "target": "chat:1", "markdown": "   " })
        )
        .is_err());

        assert!(validate_tool_args("create_chat", &json!({ "members": [] })).is_err());
        assert!(
            validate_tool_args("create_chat", &json!({ "members": ["a@b.com"] })).is_ok()
        );

        for removed in ["react_message", "mark_read", "set_presence"] {
            assert!(
                validate_tool_args(removed, &json!({})).is_err(),
                "{removed} should not be exposed as an MCP tool"
            );
        }
    }

    #[test]
    fn edited_and_compact_tools_validate_their_options() {
        for arguments in [
            json!({"target": "chat:c", "msg_id": "m", "markdown": "Full"}),
            json!({"target": "chat:c", "msg_id": "m", "find": "old", "replace": ""}),
            json!({"target": "chan:t:c", "msg_id": "m", "subject": "New title"}),
        ] {
            validate_tool_args("edit_message", &arguments).unwrap();
        }
        for arguments in [
            json!({"target": "chat:c", "msg_id": "m", "markdown": "Full", "subject": "title"}),
            json!({"target": "chat:c", "msg_id": "m", "markdown": "Full", "find": "old", "replace": ""}),
            json!({"target": "chat:c", "msg_id": "m", "find": "old"}),
            json!({"target": "chat:c", "msg_id": "m", "find": "", "replace": "new"}),
            json!({"target": "chat:c", "msg_id": "m", "find": "old", "replace": null}),
            json!({"target": "chan:t:c", "msg_id": "m", "subject": "New title", "thread_root_id": "root"}),
            json!({"target": "chan:t:c", "msg_id": "m", "subject": " "}),
        ] {
            assert!(validate_tool_args("edit_message", &arguments).is_err(), "{arguments}");
        }
        validate_tool_args(
            "get_thread",
            &json!({"target": "chan:t:c", "root_msg_id": "m", "offline": true, "compact": true}),
        )
        .unwrap();
        assert!(validate_tool_args(
            "get_thread",
            &json!({"target": "chan:t:c", "root_msg_id": "m", "offline": "true"}),
        )
        .is_err());
        assert!(validate_tool_args(
            "get_messages",
            &json!({"target": "chat:c", "format": "plain"}),
        )
        .is_err());
        assert!(validate_tool_args("update_chat", &json!({
            "target": "chat:c", "topic": "New topic", "members": []
        })).is_err());
        validate_tool_args(
            "search_users",
            &json!({"query": "Alice", "limit": 5}),
        )
        .unwrap();
        assert!(validate_tool_args("search_users", &json!({})).is_err());
        assert!(validate_tool_args("search_users", &json!({"query": ""})).is_err());
        assert!(validate_tool_args(
            "search_users",
            &json!({"query": "Alice", "extra": true}),
        )
        .is_err());
        validate_tool_args(
            "send_message",
            &json!({"target": "chat:c", "markdown": "hi", "compact": false}),
        )
        .unwrap();
        assert!(validate_tool_args(
            "send_message",
            &json!({"target": "chat:c", "markdown": "hi", "compact": "false"}),
        )
        .is_err());
    }

    #[tokio::test]
    async fn offline_get_messages_before_and_search_messages_work_against_sqlite() {
        use crate::models::{ConversationKind, MessageItem};

        let (cfg, graph, storage) = test_ctx();
        storage
            .upsert_conversations(&[ConversationSummary {
                conv_key: "chat:19:c1".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "19:c1".to_string(),
                },
                kind: ConversationKind::Group,
                team_id: None,
                team_name: None,
                title: "Engineering Discussion".to_string(),
                topic: Some("Engineering Discussion".to_string()),
                unread_count: 0,
                last_read_at: None,
                last_msg_id: Some("msg-2".to_string()),
                last_msg_author: Some("Bob".to_string()),
                last_msg_preview: Some("Check release tag".to_string()),
                last_updated_at: Some("2026-11-15T06:00:00Z".to_string()),
                pinned: false,
                peer_user_id: None,
                web_url: None,
            }])
            .unwrap();
        storage
            .upsert_messages(&[
                MessageItem {
                    msg_key: "chat:19:c1|msg-1".to_string(),
                    conv_key: "chat:19:c1".to_string(),
                    msg_id: "msg-1".to_string(),
                    parent_msg_id: None,
                    reply_to_id: None,
                    reply_to_author: None,
                    reply_to_preview: None,
                    subject: None,
                    author_id: Some("u1".to_string()),
                    author_name: "Bob".to_string(),
                    is_me: false,
                    created_at: "2026-11-10T06:00:00Z".to_string(),
                    last_modified_at: None,
                    deleted: false,
                    importance: "normal".to_string(),
                    message_type: "message".to_string(),
                    body_html: "<p>Older message</p>".to_string(),
                    body_plain: "Older message".to_string(),
                    attachments: vec![],
                    mentions: vec![],
                    reactions: vec![],
                    reply_count: 0,
                    web_url: None,
                },
                MessageItem {
                    msg_key: "chat:19:c1|msg-2".to_string(),
                    conv_key: "chat:19:c1".to_string(),
                    msg_id: "msg-2".to_string(),
                    parent_msg_id: None,
                    reply_to_id: None,
                    reply_to_author: None,
                    reply_to_preview: None,
                    subject: None,
                    author_id: Some("u1".to_string()),
                    author_name: "Bob".to_string(),
                    is_me: false,
                    created_at: "2026-11-15T06:00:00Z".to_string(),
                    last_modified_at: None,
                    deleted: false,
                    importance: "normal".to_string(),
                    message_type: "message".to_string(),
                    body_html: "<p>Please check the release tag</p>".to_string(),
                    body_plain: "Please check the release tag".to_string(),
                    attachments: vec![],
                    mentions: vec![],
                    reactions: vec![],
                    reply_count: 0,
                    web_url: None,
                },
            ])
            .unwrap();

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 20,
                "method": "tools/call",
                "params": {
                    "name": "get_messages",
                    "arguments": { "target": "Engineering Discussion", "offline": true, "before": "msg-2" }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["messages"].as_array().unwrap().len(), 1);
        assert_eq!(payload["messages"][0]["msg_id"], "msg-1");
        assert_eq!(payload["has_more"], false);
        assert_eq!(payload["next_before"], "msg-1");

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 21,
                "method": "tools/call",
                "params": {
                    "name": "search_messages",
                    "arguments": { "query": "release", "target": "Engineering Discussion" }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["hits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn reply_message_supports_mentions_and_important() {
        let catalog = tool_catalog();
        let reply_tool = catalog
            .iter()
            .find(|t| t["name"] == "reply_message")
            .unwrap();
        assert!(reply_tool["inputSchema"]["properties"]["mentions"].is_object());
        assert!(reply_tool["inputSchema"]["properties"]["important"].is_object());

        assert!(validate_tool_args(
            "reply_message",
            &json!({
                "target": "Engineering/General",
                "msg_id": "m1",
                "markdown": "Thanks @Alice",
                "mentions": ["u1:Alice"],
                "important": true
            })
        )
        .is_ok());

        assert!(validate_tool_args(
            "reply_message",
            &json!({
                "target": "Engineering/General",
                "msg_id": "m1",
                "markdown": "Thanks",
                "important": "yes"
            })
        )
        .is_err());
    }

    #[tokio::test]
    async fn bounded_line_reader_caps_oversized_frames() {
        let data = b"hello\r\n0123456789ABCDEF\nworld";
        let mut reader = BufReader::new(std::io::Cursor::new(&data[..]));

        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Line(s) => assert_eq!(s, "hello"),
            _ => panic!("expected first line"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::TooLong => {}
            _ => panic!("expected TooLong"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Line(s) => assert_eq!(s, "world"),
            _ => panic!("expected trailing line at EOF"),
        }
        match read_line_bounded(&mut reader, 10).await.unwrap() {
            BoundedLine::Eof => {}
            _ => panic!("expected Eof"),
        }
    }

    #[tokio::test]
    async fn cached_inline_hosted_image_exposed_in_mcp_get_messages_and_download() {
        use crate::auth::StoredTokens;
        use crate::models::{ConversationKind, MessageItem};
        use axum::{http::StatusCode, routing::get, Router};

        let png_bytes = b"\x89PNG\r\n\x1a\nmcp-inline-shot".to_vec();
        let served_bytes = png_bytes.clone();
        let mock_app = Router::new().route(
            "/v1.0/chats/c1/messages/m2/hostedContents/1/$value",
            get(move || {
                let bytes = served_bytes.clone();
                async move { (StatusCode::OK, bytes) }
            }),
        );
        let mock_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_listener.local_addr().unwrap();
        let mock_server = tokio::spawn(async move {
            axum::serve(mock_listener, mock_app).await.unwrap();
        });

        let dl_dir = tempfile::tempdir().unwrap();
        let cfg = AppConfig {
            graph_base_url: format!("http://{mock_addr}/v1.0"),
            download_dir: dl_dir.path().display().to_string(),
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
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        storage
            .upsert_conversations(&[ConversationSummary {
                conv_key: "chat:c1".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "c1".to_string(),
                },
                kind: ConversationKind::OneOnOne,
                team_id: None,
                team_name: None,
                title: "MCP Inline Chat".to_string(),
                topic: None,
                unread_count: 0,
                last_read_at: None,
                last_msg_id: Some("m2".to_string()),
                last_msg_author: Some("Alice".to_string()),
                last_msg_preview: Some("Inline".to_string()),
                last_updated_at: Some("2026-11-18T10:01:00Z".to_string()),
                pinned: false,
                peer_user_id: None,
                web_url: None,
            }])
            .unwrap();

        let graph_hosted_url =
            "https://graph.microsoft.com/v1.0/chats/c1/messages/m1/hostedContents/1/$value";
        let local_hosted_url =
            format!("http://{mock_addr}/v1.0/chats/c1/messages/m2/hostedContents/1/$value");

        storage
            .upsert_messages(&[
                MessageItem {
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
                    body_html: format!(r#"<p>Inline</p><img src="{graph_hosted_url}">"#),
                    body_plain: "Inline".to_string(),
                    attachments: vec![],
                    mentions: vec![],
                    reactions: vec![],
                    reply_count: 0,
                    web_url: None,
                },
                MessageItem {
                    msg_key: "chat:c1|m2".to_string(),
                    conv_key: "chat:c1".to_string(),
                    msg_id: "m2".to_string(),
                    parent_msg_id: None,
                    reply_to_id: None,
                    reply_to_author: None,
                    reply_to_preview: None,
                    subject: None,
                    author_id: Some("u1".to_string()),
                    author_name: "Alice".to_string(),
                    is_me: false,
                    created_at: "2026-11-18T10:01:00Z".to_string(),
                    last_modified_at: None,
                    deleted: false,
                    importance: "normal".to_string(),
                    message_type: "message".to_string(),
                    body_html: format!(r#"<img src="{local_hosted_url}">"#),
                    body_plain: String::new(),
                    attachments: vec![],
                    mentions: vec![],
                    reactions: vec![],
                    reply_count: 0,
                    web_url: None,
                },
            ])
            .unwrap();

        // MCP get_messages (offline) hydrates hosted:1 attachment
        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 30,
                "method": "tools/call",
                "params": {
                    "name": "get_messages",
                    "arguments": { "target": "chat:c1", "offline": true, "compact": false }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(payload["messages"][0]["attachments"][0]["id"], "hosted:1");
        assert_eq!(
            payload["messages"][0]["attachments"][0]["content_url"],
            graph_hosted_url
        );

        // MCP download_attachment downloads hosted:1 from cached message
        let dl_resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 31,
                "method": "tools/call",
                "params": {
                    "name": "download_attachment",
                    "arguments": { "target": "chat:c1", "msg_id": "m2", "index": 0 }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(dl_resp["result"]["isError"], false, "{dl_resp}");
        let dl_payload: Value =
            serde_json::from_str(dl_resp["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(dl_payload["ok"], true);
        assert_eq!(dl_payload["bytes_written"], png_bytes.len());
        assert_eq!(dl_payload["size_bytes"], png_bytes.len());
        assert_eq!(dl_payload["mime_type"], "image/png");
        assert_eq!(dl_payload["is_image"], true);
        let saved_path = dl_payload["path"].as_str().unwrap();
        assert!(std::path::Path::new(saved_path).is_absolute());
        assert_eq!(std::fs::read(saved_path).unwrap(), png_bytes);

        mock_server.abort();
    }

    #[test]
    fn paged_list_tools_validate_limit_and_cursor() {
        validate_tool_args(
            "list_members",
            &json!({ "target": "chat:c1", "limit": 5, "cursor": "10-0", "offline": true }),
        )
        .unwrap();
        assert!(validate_tool_args("list_members", &json!({ "target": "chat:c1", "limit": 0 })).is_err());
        assert!(validate_tool_args("list_members", &json!({ "target": "chat:c1", "limit": 1001 })).is_err());
        assert!(validate_tool_args("list_members", &json!({ "target": "chat:c1", "cursor": 5 })).is_err());
        validate_tool_args(
            "list_teams_channels",
            &json!({ "limit": 50, "cursor": "0-0" }),
        )
        .unwrap();
        assert!(validate_tool_args("list_teams_channels", &json!({ "bogus": 1 })).is_err());
        assert!(validate_tool_args(
            "list_teams_channels",
            &json!({ "refresh": true, "cursor": "0-0" })
        )
        .is_err());
    }

    #[tokio::test]
    async fn compact_get_messages_includes_non_empty_attachment_content_type() {
        use crate::models::{AttachmentInfo, ConversationKind, MessageItem};

        let (cfg, graph, storage) = test_ctx();
        storage
            .upsert_conversations(&[ConversationSummary {
                conv_key: "chat:19:cards".to_string(),
                target: ConversationTarget::Chat {
                    chat_id: "19:cards".to_string(),
                },
                kind: ConversationKind::Group,
                team_id: None,
                team_name: None,
                title: "Alerts Bot".to_string(),
                topic: None,
                unread_count: 0,
                last_read_at: None,
                last_msg_id: Some("msg-card".to_string()),
                last_msg_author: Some("AlertBot".to_string()),
                last_msg_preview: Some("Deploy alert".to_string()),
                last_updated_at: Some("2026-11-20T09:00:00Z".to_string()),
                pinned: false,
                peer_user_id: None,
                web_url: None,
            }])
            .unwrap();
        storage
            .upsert_messages(&[MessageItem {
                msg_key: "chat:19:cards|msg-card".to_string(),
                conv_key: "chat:19:cards".to_string(),
                msg_id: "msg-card".to_string(),
                parent_msg_id: None,
                reply_to_id: None,
                reply_to_author: None,
                reply_to_preview: None,
                subject: None,
                author_id: Some("bot-1".to_string()),
                author_name: "AlertBot".to_string(),
                is_me: false,
                created_at: "2026-11-20T09:00:00Z".to_string(),
                last_modified_at: None,
                deleted: false,
                importance: "normal".to_string(),
                message_type: "message".to_string(),
                body_html: String::new(),
                body_plain: "Deploy alert".to_string(),
                attachments: vec![
                    AttachmentInfo {
                        id: "card-att-1".to_string(),
                        content_type: "application/vnd.microsoft.card.adaptive".to_string(),
                        content_url: None,
                        name: None,
                        content_json: Some(r#"{"type":"AdaptiveCard"}"#.to_string()),
                        thumbnail_url: None,
                    },
                    AttachmentInfo {
                        id: "unnamed-att-2".to_string(),
                        content_type: String::new(),
                        content_url: None,
                        name: None,
                        content_json: None,
                        thumbnail_url: None,
                    },
                ],
                mentions: vec![],
                reactions: vec![],
                reply_count: 0,
                web_url: None,
            }])
            .unwrap();

        let resp = handle_rpc_line(
            &cfg,
            &graph,
            &storage,
            &json!({
                "jsonrpc": "2.0",
                "id": 40,
                "method": "tools/call",
                "params": {
                    "name": "get_messages",
                    "arguments": { "target": "chat:19:cards", "offline": true }
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
        assert_eq!(resp["result"]["isError"], false, "{resp}");
        let payload: Value =
            serde_json::from_str(resp["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            payload["messages"][0]["attachments"],
            json!([
                {
                    "id": "card-att-1",
                    "name": null,
                    "content_type": "application/vnd.microsoft.card.adaptive"
                },
                {
                    "id": "unnamed-att-2",
                    "name": null
                }
            ])
        );
    }
}
