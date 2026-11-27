//! MCP tool handlers for chats, teams, and channels.

use serde_json::{json, Value};

use crate::{
    cli,
    graph::GraphClient,
    models::ConversationSummary,
    storage::Storage,
};

use super::{
    format_error, map_graph_result,
    validation::{bounds, Args},
    with_storage,
};

pub(super) async fn list_chats(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn create_chat(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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

pub(super) async fn update_chat(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
    let target = args.required_str("target")?;
    let topic = args.required_text("topic")?;
    let chat = map_graph_result(
        graph,
        cli::update_group_chat_topic(storage, graph, &target, &topic).await,
    )
    .await?;
    Ok(json!({ "ok": true, "conversation": chat }))
}

pub(super) async fn list_teams_channels(
    graph: &GraphClient,
    storage: &Storage,
    args: &Args<'_>,
) -> Result<Value, String> {
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
