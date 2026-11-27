//! MCP tool handlers for reading, sending, replying, editing, deleting, and attachments.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::{
    cli,
    config::AppConfig,
    graph::GraphClient,
    parser,
    storage::Storage,
};

use super::{
    format_error, map_graph_result, resolve_target_mcp,
    validation::{bounds, Args},
    with_storage,
};

pub(super) async fn get_messages(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn get_thread(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn send_message(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn reply_message(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn send_file(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn download_attachment(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn edit_message(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn delete_message(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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
