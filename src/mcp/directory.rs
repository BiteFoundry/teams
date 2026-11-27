//! MCP tool handlers for search, members, directory users, and presence.

use serde_json::{json, Value};

use crate::{
    cli,
    graph::GraphClient,
    resolver,
    storage::Storage,
};

use super::{
    format_error, map_graph_result, resolve_target_mcp,
    validation::{bounds, Args},
    with_storage,
};

pub(super) async fn search_messages(
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn list_members(
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

pub(super) async fn search_users(
    graph: &GraphClient,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn get_presence(
    graph: &GraphClient,
    args: &Args<'_>,
) -> Result<Value, String> {
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
