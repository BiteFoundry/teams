//! Microsoft Graph v1.0 endpoints for Joined Teams, Channels, Delta Sync, Threads,
//! and unified Send / Reply / Edit / Delete / Reaction operations.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use futures::StreamExt;
use serde::Deserialize;

use super::{
    chats::GraphConversationMember,
    client::{
        is_pure_system_noise, normalize_chat_message, sort_messages_chronologically,
        truncate_to_newest, BatchSubRequest, GraphChatMessage, GraphDeltaPage, GraphListPage,
    },
    GraphClient,
};
use crate::models::{
    ChannelSummary, ConversationTarget, JoinedTeams, MemberInfo, MessageItem, OutgoingMessage,
    QuotedReply, TeamChannelError, TeamSummary, ThreadView,
};

#[derive(Debug, Deserialize)]
struct GraphTeam {
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphChannel {
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "membershipType", default)]
    membership_type: Option<String>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
}

impl GraphChannel {
    fn into_summary(self, team_id: &str, team_name: &str) -> ChannelSummary {
        ChannelSummary {
            id: self.id.clone(),
            team_id: team_id.to_string(),
            team_name: team_name.to_string(),
            display_name: self
                .display_name
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(self.id),
            description: self.description.filter(|s| !s.trim().is_empty()),
            membership_type: self
                .membership_type
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "standard".to_string()),
            web_url: self.web_url,
        }
    }
}

/// `/teams/{id}/allChannels` also returns channels shared into the team from other teams.
const ALL_CHANNELS_SEGMENT: &str = "allChannels";
/// `/teams/{id}/channels` returns only the team's own channels.
const OWN_CHANNELS_SEGMENT: &str = "channels";

fn team_channels_url(team_id: &str, segment: &str) -> String {
    format!(
        "/teams/{}/{segment}?$select=id,displayName,description,membershipType,webUrl",
        urlencoding::encode(team_id)
    )
}

/// `/allChannels` is unavailable (403: shared channels not permitted, 404: not supported)
/// rather than failing transiently, so `/channels` is the listing to use.
fn all_channels_unavailable(status: Option<u16>) -> bool {
    matches!(status, Some(403 | 404))
}

impl GraphClient {
    pub async fn list_joined_teams_with_channels(&self) -> Result<JoinedTeams> {
        let mut next_url = Some("/me/joinedTeams?$select=id,displayName,description".to_string());
        let mut teams = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphTeam> = self.get_json(&url).await?;
            teams.extend(page.value);
            next_url = page.next_link;
        }

        if teams.is_empty() {
            return Ok(JoinedTeams {
                teams: Vec::new(),
                channel_errors: Vec::new(),
            });
        }

        let requests: Vec<BatchSubRequest> = teams
            .iter()
            .enumerate()
            .map(|(idx, team)| BatchSubRequest {
                id: idx.to_string(),
                method: "GET".to_string(),
                url: team_channels_url(&team.id, ALL_CHANNELS_SEGMENT),
                body: None,
                headers: None,
            })
            .collect();

        let responses = self.batch(&requests).await?;
        let mut channels_by_idx: HashMap<usize, (Vec<GraphChannel>, Option<String>)> =
            HashMap::new();
        let mut failed: HashMap<usize, (Option<u16>, String)> = HashMap::new();
        for resp in responses {
            let Ok(idx) = resp.id.parse::<usize>() else {
                continue;
            };
            if !(200..300).contains(&resp.status) {
                let code = resp
                    .body
                    .pointer("/error/code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let message = resp
                    .body
                    .pointer("/error/message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                failed.insert(
                    idx,
                    (Some(resp.status), format!("{} [{code}] {message}", resp.status)),
                );
                continue;
            }
            match serde_json::from_value::<GraphListPage<GraphChannel>>(resp.body) {
                Ok(page) => {
                    let next_link = page
                        .next_link
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    channels_by_idx.insert(idx, (page.value, next_link));
                }
                Err(_) => {
                    failed.insert(
                        idx,
                        (Some(resp.status), "invalid channel list payload".to_string()),
                    );
                }
            }
        }

        const TEAM_CHANNEL_FOLLOWUP_CONCURRENCY: usize = 6;
        let work: Vec<_> = teams
            .into_iter()
            .enumerate()
            .map(|(idx, team)| {
                let team_id = team.id;
                let team_name = team
                    .display_name
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| team_id.clone());
                let description = team.description.filter(|s| !s.trim().is_empty());
                let page = channels_by_idx.remove(&idx);
                let failed_status = failed.remove(&idx).and_then(|(status, _)| status);
                (team_id, team_name, description, page, failed_status)
            })
            .collect();

        let client = self.clone();
        let mut stream = futures::stream::iter(work)
            .map(|(team_id, team_name, description, page, failed_status)| {
                let client = client.clone();
                async move {
                    let (channels, channel_error) = match page {
                        Some((raw_channels, next_link)) => {
                            let channels = client
                                .collect_team_channels_from_page(
                                    &team_id,
                                    &team_name,
                                    raw_channels,
                                    next_link,
                                )
                                .await?;
                            (channels, None)
                        }
                        None => {
                            // A 403/404 from `/allChannels` means this tenant or team cannot
                            // list shared channels: go straight to `/channels` instead of
                            // asking `/allChannels` again.
                            let fetched = if all_channels_unavailable(failed_status) {
                                client.list_own_team_channels(&team_id, &team_name).await
                            } else {
                                client.list_team_channels(&team_id, &team_name).await
                            };
                            match fetched {
                                Ok(channels) => (channels, None),
                                Err(err) => (
                                    Vec::new(),
                                    Some(TeamChannelError {
                                        team_id: team_id.clone(),
                                        display_name: team_name.clone(),
                                        status: failed_status,
                                        error: format!("{err:#}"),
                                    }),
                                ),
                            }
                        }
                    };
                    Ok::<_, anyhow::Error>((
                        TeamSummary {
                            id: team_id,
                            display_name: team_name,
                            description,
                            channels,
                        },
                        channel_error,
                    ))
                }
            })
            .buffered(TEAM_CHANNEL_FOLLOWUP_CONCURRENCY);

        let mut summaries = Vec::new();
        let mut channel_errors = Vec::new();
        while let Some(res) = stream.next().await {
            let (summary, err) = res?;
            if let Some(err) = err {
                channel_errors.push(err);
            }
            summaries.push(summary);
        }

        Ok(JoinedTeams {
            teams: summaries,
            channel_errors,
        })
    }

    async fn collect_team_channels_from_page(
        &self,
        team_id: &str,
        team_name: &str,
        initial: Vec<GraphChannel>,
        mut next_url: Option<String>,
    ) -> Result<Vec<ChannelSummary>> {
        let mut channels = Vec::with_capacity(initial.len());
        let mut seen_ids = HashSet::new();
        for ch in initial {
            if seen_ids.insert(ch.id.clone()) {
                channels.push(ch.into_summary(team_id, team_name));
            }
        }
        while let Some(url) = next_url {
            let page: GraphListPage<GraphChannel> = self.get_json(&url).await?;
            for ch in page.value {
                if seen_ids.insert(ch.id.clone()) {
                    channels.push(ch.into_summary(team_id, team_name));
                }
            }
            next_url = page
                .next_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }
        Ok(channels)
    }

    /// Lists a team's channels including the shared channels other teams share into it
    /// (`/allChannels`), falling back to the team's own channels (`/channels`) when Graph
    /// answers 403/404 for `/allChannels`.
    pub async fn list_team_channels(
        &self,
        team_id: &str,
        team_name: &str,
    ) -> Result<Vec<ChannelSummary>> {
        let all_url = team_channels_url(team_id, ALL_CHANNELS_SEGMENT);
        match self
            .collect_team_channels_from_page(team_id, team_name, Vec::new(), Some(all_url))
            .await
        {
            Ok(channels) => Ok(channels),
            Err(err)
                if all_channels_unavailable(crate::graph::extract_graph_http_status(&format!(
                    "{err:#}"
                ))) =>
            {
                self.list_own_team_channels(team_id, team_name).await
            }
            Err(err) => Err(err),
        }
    }

    /// The team's own channels only (`/channels`), without incoming shared channels.
    async fn list_own_team_channels(
        &self,
        team_id: &str,
        team_name: &str,
    ) -> Result<Vec<ChannelSummary>> {
        let own_url = team_channels_url(team_id, OWN_CHANNELS_SEGMENT);
        self.collect_team_channels_from_page(team_id, team_name, Vec::new(), Some(own_url))
            .await
    }

    pub async fn list_channel_members(
        &self,
        team_id: &str,
        channel_id: &str,
    ) -> Result<Vec<MemberInfo>> {
        let mut next_url = Some(format!(
            "/teams/{}/channels/{}/members",
            urlencoding::encode(team_id),
            urlencoding::encode(channel_id)
        ));
        let mut members = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphConversationMember> = self.get_json(&url).await?;
            members.extend(page.value.into_iter().map(MemberInfo::from));
            next_url = page.next_link;
        }
        Ok(members)
    }

    /// Fetches a single page of channel root posts (with inline expanded replies), automatically
    /// advancing up to 5 consecutive pages if a non-empty raw Graph page contains only system noise.
    pub async fn list_channel_messages_page(
        &self,
        team_id: &str,
        channel_id: &str,
        top: usize,
        next_link: Option<&str>,
    ) -> Result<(Vec<MessageItem>, Option<String>)> {
        let my_id = self.resolve_my_user_id().await;
        let conv_key = ConversationTarget::Channel {
            team_id: team_id.to_string(),
            channel_id: channel_id.to_string(),
        }
        .conv_key();

        let page_top = top.clamp(1, 50);
        let mut url = next_link
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "/teams/{}/channels/{}/messages?$expand=replies&$top={page_top}",
                    urlencoding::encode(team_id),
                    urlencoding::encode(channel_id)
                )
            });

        let mut items = Vec::new();
        let mut current_next_link;
        let mut empty_noise_pages = 0usize;

        loop {
            let page: GraphListPage<GraphChatMessage> = self.get_json(&url).await?;
            let raw_non_empty = !page.value.is_empty();

            for raw in page.value {
                let root_id = raw.id.clone();
                let inline_replies: Vec<GraphChatMessage> = raw
                    .replies
                    .as_deref()
                    .into_iter()
                    .flatten()
                    .filter_map(|val| serde_json::from_value::<GraphChatMessage>(val.clone()).ok())
                    .filter(is_expanded_reply_payload)
                    .collect();

                let normalized_root =
                    normalize_chat_message(&conv_key, None, my_id.as_deref(), raw);
                if !is_pure_system_noise(&normalized_root) {
                    items.push(normalized_root);
                }

                for raw_reply in inline_replies {
                    let normalized_reply = normalize_chat_message(
                        &conv_key,
                        Some(root_id.clone()),
                        my_id.as_deref(),
                        raw_reply,
                    );
                    if !is_pure_system_noise(&normalized_reply) {
                        items.push(normalized_reply);
                    }
                }
            }

            current_next_link = page
                .next_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            if items.is_empty()
                && raw_non_empty
                && current_next_link.is_some()
                && empty_noise_pages < 5
            {
                empty_noise_pages += 1;
                url = current_next_link.take().unwrap();
                continue;
            }
            break;
        }

        sort_messages_chronologically(&mut items);
        Ok((items, current_next_link))
    }

    pub async fn list_channel_messages(
        &self,
        team_id: &str,
        channel_id: &str,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut items = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut root_count = 0usize;
        let mut next_link: Option<String> = None;

        loop {
            let (page_items, next) = self
                .list_channel_messages_page(
                    team_id,
                    channel_id,
                    limit.min(50),
                    next_link.as_deref(),
                )
                .await?;
            for item in page_items {
                if seen_ids.insert(item.msg_id.clone()) {
                    if item.parent_msg_id.is_none() {
                        root_count += 1;
                    }
                    items.push(item);
                }
            }
            if root_count >= limit || next.is_none() {
                break;
            }
            next_link = next;
        }

        sort_messages_chronologically(&mut items);
        if root_count > limit {
            let mut roots: Vec<MessageItem> = items
                .iter()
                .filter(|m| m.parent_msg_id.is_none())
                .cloned()
                .collect();
            truncate_to_newest(&mut roots, limit);
            let kept_root_ids: HashSet<String> =
                roots.iter().map(|r| r.msg_id.clone()).collect();
            items.retain(|m| match m.parent_msg_id.as_deref() {
                None => kept_root_ids.contains(&m.msg_id),
                Some(pid) => kept_root_ids.contains(pid),
            });
        }
        Ok(items)
    }

    /// Incremental channel sync using `/teams/{id}/channels/{id}/messages/delta`.
    /// Returns `(changed_messages, new_delta_link)`.
    pub async fn sync_channel_messages_delta(
        &self,
        team_id: &str,
        channel_id: &str,
        delta_link: Option<&str>,
    ) -> Result<(Vec<MessageItem>, Option<String>)> {
        let my_id = self.resolve_my_user_id().await;
        let conv_key = ConversationTarget::Channel {
            team_id: team_id.to_string(),
            channel_id: channel_id.to_string(),
        }
        .conv_key();

        let mut url = delta_link
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "/teams/{}/channels/{}/messages/delta?$top=50",
                    urlencoding::encode(team_id),
                    urlencoding::encode(channel_id)
                )
            });

        let mut all_changed: Vec<MessageItem> = Vec::new();
        let mut msg_pos_by_id: HashMap<String, usize> = HashMap::new();
        let mut final_delta_link = None;

        loop {
            let page: GraphDeltaPage<GraphChatMessage> = self.get_json(&url).await?;
            for raw in page.value {
                let parent_id = raw
                    .reply_to_id
                    .as_deref()
                    .filter(|rid| !rid.is_empty() && *rid != raw.id)
                    .map(str::to_string);
                let mut normalized =
                    normalize_chat_message(&conv_key, parent_id, my_id.as_deref(), raw);
                if !is_pure_system_noise(&normalized) {
                    if let Some(&pos) = msg_pos_by_id.get(&normalized.msg_id) {
                        let prev = &all_changed[pos];
                        if normalized.deleted && normalized.created_at.is_empty() {
                            normalized.created_at = prev.created_at.clone();
                        }
                        if normalized.deleted && normalized.author_name == "Unknown" {
                            normalized.author_name = prev.author_name.clone();
                            normalized.author_id = prev.author_id.clone();
                            normalized.is_me = prev.is_me;
                        }
                        all_changed[pos] = normalized;
                    } else {
                        let idx = all_changed.len();
                        msg_pos_by_id.insert(normalized.msg_id.clone(), idx);
                        all_changed.push(normalized);
                    }
                }
            }
            if let Some(dl) = page
                .delta_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                final_delta_link = Some(dl);
                break;
            }
            if let Some(nl) = page
                .next_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                url = nl;
            } else {
                break;
            }
        }

        sort_messages_chronologically(&mut all_changed);
        Ok((all_changed, final_delta_link))
    }

    pub async fn list_thread_replies(
        &self,
        team_id: &str,
        channel_id: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        let (replies, _) = self
            .fetch_thread_replies(team_id, channel_id, root_msg_id, limit)
            .await?;
        Ok(replies)
    }

    async fn fetch_thread_replies(
        &self,
        team_id: &str,
        channel_id: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<(Vec<MessageItem>, bool)> {
        if limit == 0 {
            return Ok((Vec::new(), false));
        }
        let my_id = self.resolve_my_user_id().await;
        let conv_key = ConversationTarget::Channel {
            team_id: team_id.to_string(),
            channel_id: channel_id.to_string(),
        }
        .conv_key();

        const MAX_THREAD_REPLY_PAGES: usize = 20;
        let top = 50usize;
        let mut next_url = Some(format!(
            "/teams/{}/channels/{}/messages/{}/replies?$top={top}",
            urlencoding::encode(team_id),
            urlencoding::encode(channel_id),
            urlencoding::encode(root_msg_id)
        ));
        let mut replies = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut pages_fetched = 0usize;
        let max_pages = limit.saturating_add(49).div_ceil(50).max(MAX_THREAD_REPLY_PAGES);

        while let Some(url) = next_url.take() {
            pages_fetched += 1;
            let page: GraphListPage<GraphChatMessage> = self.get_json(&url).await?;
            for raw in page.value {
                let normalized = normalize_chat_message(
                    &conv_key,
                    Some(root_msg_id.to_string()),
                    my_id.as_deref(),
                    raw,
                );
                if !is_pure_system_noise(&normalized) && seen_ids.insert(normalized.msg_id.clone())
                {
                    replies.push(normalized);
                }
            }
            next_url = page
                .next_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            if pages_fetched >= max_pages {
                break;
            }
        }

        let complete = next_url.is_none() && replies.len() <= limit;
        sort_messages_chronologically(&mut replies);
        truncate_to_newest(&mut replies, limit);
        Ok((replies, complete))
    }

    pub async fn get_thread(
        &self,
        team_id: &str,
        channel_id: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<ThreadView> {
        let (thread, _) = self
            .get_thread_with_coverage(team_id, channel_id, root_msg_id, limit)
            .await?;
        Ok(thread)
    }

    /// The flag indicates that the returned replies cover the whole thread.
    pub(crate) async fn get_thread_with_coverage(
        &self,
        team_id: &str,
        channel_id: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<(ThreadView, bool)> {
        let my_id = self.resolve_my_user_id().await;
        let conv_key = ConversationTarget::Channel {
            team_id: team_id.to_string(),
            channel_id: channel_id.to_string(),
        }
        .conv_key();

        let root_path = format!(
            "/teams/{}/channels/{}/messages/{}",
            urlencoding::encode(team_id),
            urlencoding::encode(channel_id),
            urlencoding::encode(root_msg_id)
        );
        let (root_raw, (replies, complete)) = tokio::try_join!(
            self.get_json::<GraphChatMessage>(&root_path),
            self.fetch_thread_replies(team_id, channel_id, root_msg_id, limit)
        )?;

        let mut root = normalize_chat_message(&conv_key, None, my_id.as_deref(), root_raw);
        root.reply_count = root.reply_count.max(replies.len());

        Ok((
            ThreadView {
                conv_key,
                root,
                replies,
            },
            complete,
        ))
    }

    /// Fetches a single message by ID from Graph:
    /// - Chat: `/chats/{chat_id}/messages/{msg_id}`
    /// - Channel root: `/teams/{team_id}/channels/{channel_id}/messages/{msg_id}`
    /// - Channel reply: `/teams/{team_id}/channels/{channel_id}/messages/{parent_msg_id}/replies/{msg_id}`
    pub async fn get_message(
        &self,
        target: &ConversationTarget,
        msg_id: &str,
        parent_msg_id: Option<&str>,
    ) -> Result<MessageItem> {
        let my_id = self.resolve_my_user_id().await;
        let parent_for_path = match target {
            ConversationTarget::Channel { .. } => {
                parent_msg_id.map(str::trim).filter(|s| !s.is_empty())
            }
            ConversationTarget::Chat { .. } => None,
        };
        let path = message_item_path(target, msg_id, parent_for_path);
        let raw: GraphChatMessage = self.get_json(&path).await?;
        Ok(normalize_chat_message(
            &target.conv_key(),
            parent_for_path.map(str::to_string),
            my_id.as_deref(),
            raw,
        ))
    }

    /// Sends a root message to a Chat or Team Channel.
    pub async fn send_message(
        &self,
        target: &ConversationTarget,
        outgoing: &OutgoingMessage,
    ) -> Result<MessageItem> {
        let my_id = self.resolve_my_user_id().await;
        let quote_prefix = outgoing
            .quote_reply_to
            .as_ref()
            .map(build_quote_blockquote_html);
        let payload = build_chat_message_payload(outgoing, quote_prefix.as_deref());

        let path = match target {
            ConversationTarget::Chat { chat_id } => {
                format!("/chats/{}/messages", urlencoding::encode(chat_id))
            }
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => format!(
                "/teams/{}/channels/{}/messages",
                urlencoding::encode(team_id),
                urlencoding::encode(channel_id)
            ),
        };

        let created: GraphChatMessage = self.post_json(&path, &payload).await?;
        Ok(normalize_chat_message(
            &target.conv_key(),
            None,
            my_id.as_deref(),
            created,
        ))
    }

    /// Replies to a message:
    /// - For a Team Channel, posts a native thread reply under `root_or_target_msg_id` (`/messages/{id}/replies`).
    /// - For a 1:1 / Group Chat, sends a message quoting `root_or_target_msg_id`.
    pub async fn reply_message(
        &self,
        target: &ConversationTarget,
        root_or_target_msg_id: &str,
        outgoing: &OutgoingMessage,
    ) -> Result<MessageItem> {
        let my_id = self.resolve_my_user_id().await;
        match target {
            ConversationTarget::Channel {
                team_id,
                channel_id,
            } => {
                let payload = build_chat_message_payload(outgoing, None);
                let path = format!(
                    "/teams/{}/channels/{}/messages/{}/replies",
                    urlencoding::encode(team_id),
                    urlencoding::encode(channel_id),
                    urlencoding::encode(root_or_target_msg_id)
                );
                let created: GraphChatMessage = self.post_json(&path, &payload).await?;
                Ok(normalize_chat_message(
                    &target.conv_key(),
                    Some(root_or_target_msg_id.to_string()),
                    my_id.as_deref(),
                    created,
                ))
            }
            ConversationTarget::Chat { chat_id } => {
                let quote = if let Some(ref q) = outgoing.quote_reply_to {
                    q.clone()
                } else {
                    let msg_path = format!(
                        "/chats/{}/messages/{}",
                        urlencoding::encode(chat_id),
                        urlencoding::encode(root_or_target_msg_id)
                    );
                    let orig: GraphChatMessage = self
                        .get_json(&msg_path)
                        .await
                        .with_context(|| {
                            format!(
                                "Cannot reply: message {root_or_target_msg_id} was not found in this chat"
                            )
                        })?;
                    let norm = normalize_chat_message(
                        &target.conv_key(),
                        None,
                        my_id.as_deref(),
                        orig,
                    );
                    QuotedReply {
                        msg_id: root_or_target_msg_id.to_string(),
                        author_name: norm.author_name,
                        preview_plain: norm.body_plain,
                    }
                };
                let quote_html = build_quote_blockquote_html(&quote);
                let payload = build_chat_message_payload(outgoing, Some(&quote_html));
                let path = format!("/chats/{}/messages", urlencoding::encode(chat_id));
                let created: GraphChatMessage = self.post_json(&path, &payload).await?;
                Ok(normalize_chat_message(
                    &target.conv_key(),
                    None,
                    my_id.as_deref(),
                    created,
                ))
            }
        }
    }

    pub async fn edit_message(
        &self,
        target: &ConversationTarget,
        msg_id: &str,
        parent_msg_id: Option<&str>,
        new_body_html: &str,
    ) -> Result<()> {
        let path = message_item_path(target, msg_id, parent_msg_id);
        let body = serde_json::json!({
            "body": {
                "contentType": "html",
                "content": new_body_html,
            }
        });
        self.patch_json(&path, &body).await
    }

    /// Edits one occurrence in the current Graph body and fetches the saved message.
    pub(crate) async fn replace_message_text(
        &self,
        target: &ConversationTarget,
        msg_id: &str,
        parent_msg_id: Option<&str>,
        find: &str,
        replacement: &str,
    ) -> Result<MessageItem> {
        use crate::parser::edit_text::{replace_html_text, unique_match};

        let path = message_item_path(target, msg_id, parent_msg_id);
        let original: GraphChatMessage = self.get_json(&path).await?;
        if original.id != msg_id
            || original.deleted_date_time.is_some()
            || original.removed.is_some()
        {
            bail!("The requested message does not have an editable body.");
        }
        let body = original.body.as_ref().context("Message has no body.")?;
        let content = body.content.as_deref().context("Message has no body content.")?;
        let content_type = body.content_type.as_deref().unwrap_or("html");
        let edited = if content_type.eq_ignore_ascii_case("html") {
            replace_html_text(content, find, replacement)?
        } else if content_type.eq_ignore_ascii_case("text") {
            let span = unique_match(content, find)?;
            let mut edited = content.to_owned();
            edited.replace_range(span, replacement);
            edited
        } else {
            bail!("Unsupported Graph message body content type: {content_type}");
        };

        let saved = if edited == content {
            original
        } else {
            let payload = serde_json::json!({
                "body": { "contentType": content_type, "content": edited }
            });
            self.patch_message_and_read(&path, &original, &payload).await?
        };
        let my_id = self.resolve_my_user_id().await;
        let parent = match target {
            ConversationTarget::Channel { .. } => parent_msg_id.map(str::to_owned),
            ConversationTarget::Chat { .. } => None,
        };
        Ok(normalize_chat_message(&target.conv_key(), parent, my_id.as_deref(), saved))
    }

    pub(crate) async fn edit_channel_post_subject(
        &self,
        target: &ConversationTarget,
        msg_id: &str,
        subject: &str,
    ) -> Result<MessageItem> {
        if !matches!(target, ConversationTarget::Channel { .. }) {
            bail!("Subject edits require a team channel root post.");
        }
        let path = message_item_path(target, msg_id, None);
        let (my_id, original) = tokio::try_join!(
            self.require_my_user_id(),
            self.get_json::<GraphChatMessage>(&path),
        )?;
        if original.id != msg_id
            || original.reply_to_id.is_some()
            || original.deleted_date_time.is_some()
            || original.removed.is_some()
            || original.message_type.as_deref().is_some_and(|kind| kind != "message")
        {
            bail!("Subject edits require an existing team channel root post.");
        }
        let author_id = original.from.as_ref()
            .and_then(|from| from.user.as_ref())
            .and_then(|author| author.id.as_deref());
        if !author_id.is_some_and(|id| id.eq_ignore_ascii_case(&my_id)) {
            bail!("Only the author can edit the subject of this channel post.");
        }

        let payload = serde_json::json!({ "subject": subject });
        let saved = self.patch_message_and_read(&path, &original, &payload).await?;
        if saved.subject.as_deref() != Some(subject) {
            bail!("Graph accepted the message edit, but the follow-up read did not confirm the requested subject.");
        }
        Ok(normalize_chat_message(&target.conv_key(), None, Some(&my_id), saved))
    }

    async fn patch_message_and_read(
        &self,
        path: &str,
        original: &GraphChatMessage,
        payload: &serde_json::Value,
    ) -> Result<GraphChatMessage> {
        let headers: Vec<_> = original
            .etag
            .as_deref()
            .filter(|etag| !etag.is_empty() && !etag.starts_with("W/"))
            .map(|etag| {
                let quoted = if etag.starts_with('"') {
                    etag.to_owned()
                } else {
                    format!("\"{etag}\"")
                };
                ("If-Match", quoted)
            })
            .into_iter()
            .collect();
        self.patch_json_with_headers(path, payload, &headers).await?;
        let saved: GraphChatMessage = self.get_json(path).await.context(
            "Graph accepted the message edit, but the follow-up read failed",
        )?;
        if saved.id != original.id {
            bail!("Graph accepted the message edit, but the follow-up read returned a different message ID.");
        }
        Ok(saved)
    }

    pub async fn delete_message(
        &self,
        target: &ConversationTarget,
        msg_id: &str,
        parent_msg_id: Option<&str>,
    ) -> Result<()> {
        let path = match target {
            ConversationTarget::Chat { chat_id } => {
                let me_id = self.require_my_user_id().await?;
                format!(
                    "/users/{}/chats/{}/messages/{}/softDelete",
                    urlencoding::encode(&me_id),
                    urlencoding::encode(chat_id),
                    urlencoding::encode(msg_id)
                )
            }
            ConversationTarget::Channel { .. } => {
                format!("{}/softDelete", message_item_path(target, msg_id, parent_msg_id))
            }
        };
        self.post_empty(&path).await
    }

    pub async fn set_reaction(
        &self,
        target: &ConversationTarget,
        msg_id: &str,
        parent_msg_id: Option<&str>,
        reaction_type: &str,
        enable: bool,
    ) -> Result<()> {
        let action = if enable {
            "setReaction"
        } else {
            "unsetReaction"
        };
        let path = format!(
            "{}/{}",
            message_item_path(target, msg_id, parent_msg_id),
            action
        );
        let body = serde_json::json!({ "reactionType": reaction_type });
        self.post_json_empty(&path, &body).await
    }
}

fn is_expanded_reply_payload(raw: &GraphChatMessage) -> bool {
    !raw.id.trim().is_empty()
        && (raw.created_date_time.is_some()
            || raw.deleted_date_time.is_some()
            || raw.from.is_some()
            || raw.body.is_some()
            || !raw.attachments.is_empty())
}

fn message_item_path(
    target: &ConversationTarget,
    msg_id: &str,
    parent_msg_id: Option<&str>,
) -> String {
    match target {
        ConversationTarget::Chat { chat_id } => format!(
            "/chats/{}/messages/{}",
            urlencoding::encode(chat_id),
            urlencoding::encode(msg_id)
        ),
        ConversationTarget::Channel {
            team_id,
            channel_id,
        } => {
            if let Some(pid) = parent_msg_id.filter(|s| !s.trim().is_empty()) {
                format!(
                    "/teams/{}/channels/{}/messages/{}/replies/{}",
                    urlencoding::encode(team_id),
                    urlencoding::encode(channel_id),
                    urlencoding::encode(pid),
                    urlencoding::encode(msg_id)
                )
            } else {
                format!(
                    "/teams/{}/channels/{}/messages/{}",
                    urlencoding::encode(team_id),
                    urlencoding::encode(channel_id),
                    urlencoding::encode(msg_id)
                )
            }
        }
    }
}

fn build_quote_blockquote_html(quote: &QuotedReply) -> String {
    let id_esc = escape_html(&quote.msg_id);
    let author_esc = escape_html(&quote.author_name);
    let preview_esc = escape_html(&quote.preview_plain);
    format!(
        "<blockquote itemtype=\"http://schema.skype.com/Reply\" itemid=\"{id_esc}\"><strong itemid=\"{id_esc}\">{author_esc}</strong><p>{preview_esc}</p></blockquote>"
    )
}

pub(crate) fn build_chat_message_payload(
    outgoing: &OutgoingMessage,
    quote_prefix_html: Option<&str>,
) -> serde_json::Value {
    let mut html = String::new();
    if let Some(prefix) = quote_prefix_html {
        html.push_str(prefix);
    }
    html.push_str(&outgoing.body_html);

    for att in &outgoing.attachments {
        let double_q = format!("id=\"{}\"", att.id);
        let single_q = format!("id='{}'", att.id);
        if !html.contains(&double_q) && !html.contains(&single_q) {
            html.push_str(&format!(
                "<attachment id=\"{}\"></attachment>",
                escape_html(&att.id)
            ));
        }
    }

    for img in &outgoing.inline_images {
        let src_marker = format!("../hostedContents/{}/$value", img.temp_id);
        if !html.contains(&src_marker) {
            let alt = escape_html(img.file_name.as_deref().unwrap_or("image"));
            html.push_str(&format!(
                "<p><img src=\"{src_marker}\" alt=\"{alt}\"></p>"
            ));
        }
    }

    let mut payload = serde_json::json!({
        "body": {
            "contentType": "html",
            "content": html,
        }
    });

    if let Some(subject) = outgoing
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        payload["subject"] = serde_json::Value::String(subject.to_string());
    }

    if let Some(importance) = outgoing
        .importance
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        payload["importance"] = serde_json::Value::String(importance.to_string());
    }

    if !outgoing.mentions.is_empty() {
        let mentions: Vec<serde_json::Value> = outgoing
            .mentions
            .iter()
            .map(|m| {
                serde_json::json!({
                    "id": m.id,
                    "mentionText": m.display_name,
                    "mentioned": {
                        "user": {
                            "id": m.user_id,
                            "displayName": m.display_name,
                            "userIdentityType": "aadUser"
                        }
                    }
                })
            })
            .collect();
        payload["mentions"] = serde_json::Value::Array(mentions);
    }

    if !outgoing.attachments.is_empty() {
        let attachments: Vec<serde_json::Value> = outgoing
            .attachments
            .iter()
            .map(|att| {
                serde_json::json!({
                    "id": att.id,
                    "contentType": "reference",
                    "contentUrl": att.content_url,
                    "name": att.name,
                })
            })
            .collect();
        payload["attachments"] = serde_json::Value::Array(attachments);
    }

    if !outgoing.inline_images.is_empty() {
        let hosted: Vec<serde_json::Value> = outgoing
            .inline_images
            .iter()
            .map(|img| {
                serde_json::json!({
                    "@microsoft.graph.temporaryId": img.temp_id,
                    "contentBytes": BASE64_STANDARD.encode(&img.bytes),
                    "contentType": img.mime_type,
                })
            })
            .collect();
        payload["hostedContents"] = serde_json::Value::Array(hosted);
    }

    payload
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{InlineImageUpload, MentionInfo, UploadedReferenceAttachment};

    #[test]
    fn test_build_chat_message_payload_auto_inserts_attachment_and_hosted_tags() {
        let outgoing = OutgoingMessage {
            subject: Some("Announcement".into()),
            body_html: "<p>Please check the file</p>".into(),
            mentions: vec![MentionInfo {
                id: 0,
                user_id: "u-1".into(),
                display_name: "Alice".into(),
            }],
            attachments: vec![UploadedReferenceAttachment {
                id: "153fa47d-18c9-4179-be08-9879815a9f90".into(),
                name: "report.pdf".into(),
                content_url: "https://contoso.sharepoint.com/report.pdf".into(),
                size: 1234,
                drive_item_id: Some("item-1".into()),
            }],
            inline_images: vec![InlineImageUpload {
                temp_id: "1".into(),
                mime_type: "image/png".into(),
                bytes: vec![1, 2, 3, 4],
                file_name: Some("clip.png".into()),
            }],
            importance: Some("urgent".into()),
            quote_reply_to: None,
        };

        let payload = build_chat_message_payload(&outgoing, None);
        let content = payload["body"]["content"].as_str().unwrap();
        assert!(content.contains("<attachment id=\"153fa47d-18c9-4179-be08-9879815a9f90\"></attachment>"));
        assert!(content.contains("<img src=\"../hostedContents/1/$value\" alt=\"clip.png\">"));
        assert_eq!(payload["subject"], "Announcement");
        assert_eq!(payload["importance"], "urgent");
        assert_eq!(payload["attachments"][0]["contentType"], "reference");
        assert_eq!(payload["hostedContents"][0]["@microsoft.graph.temporaryId"], "1");
        assert_eq!(payload["hostedContents"][0]["contentBytes"], "AQIDBA==");
        assert_eq!(payload["mentions"][0]["mentioned"]["user"]["id"], "u-1");
    }

    #[test]
    fn test_message_item_path_routing() {
        let chat = ConversationTarget::Chat {
            chat_id: "c1".into(),
        };
        assert_eq!(message_item_path(&chat, "m1", None), "/chats/c1/messages/m1");

        let chan = ConversationTarget::Channel {
            team_id: "t1".into(),
            channel_id: "ch1".into(),
        };
        assert_eq!(
            message_item_path(&chan, "m1", None),
            "/teams/t1/channels/ch1/messages/m1"
        );
        assert_eq!(
            message_item_path(&chan, "r1", Some("m1")),
            "/teams/t1/channels/ch1/messages/m1/replies/r1"
        );
    }

    #[tokio::test]
    async fn test_channel_messages_page_inline_replies_and_thread_multipage() {
        use axum::{extract::RawQuery, routing::get, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let ch_p2 = format!("{base}/teams/t1/channels/ch1/messages?page=2");
        let rep_p2 = format!("{base}/teams/t1/channels/ch1/messages/root-2/replies?page=2");
        let ch_p2_c = ch_p2.clone();
        let rep_p2_c = rep_p2.clone();

        let app = Router::new()
            .route(
                "/v1.0/teams/t1/channels/ch1/messages",
                get(move |RawQuery(q): RawQuery| {
                    let ch_p2 = ch_p2_c.clone();
                    async move {
                        let query = q.unwrap_or_default();
                        if query.contains("page=2") {
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "root-1",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:00:00Z",
                                        "from": { "user": { "id": "u1", "displayName": "A" } },
                                        "body": { "contentType": "text", "content": "oldest root" },
                                        "replies": [
                                            {
                                                "id": "rep-1a",
                                                "messageType": "message",
                                                "createdDateTime": "2026-11-18T10:00:30Z",
                                                "from": { "user": { "id": "u2", "displayName": "B" } },
                                                "body": { "contentType": "text", "content": "reply 1a" }
                                            }
                                        ]
                                    }
                                ]
                            }))
                        } else {
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "root-3",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:03:00Z",
                                        "from": { "user": { "id": "u1", "displayName": "A" } },
                                        "body": { "contentType": "text", "content": "newest root" },
                                        "replies": []
                                    },
                                    {
                                        "id": "root-2",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:01:00Z",
                                        "from": { "user": { "id": "u1", "displayName": "A" } },
                                        "body": { "contentType": "text", "content": "middle root" },
                                        "replies": [
                                            {
                                                "id": "rep-2a",
                                                "messageType": "message",
                                                "createdDateTime": "2026-11-18T10:02:00Z",
                                                "from": { "user": { "id": "u2", "displayName": "B" } },
                                                "body": { "contentType": "text", "content": "reply 2a" }
                                            }
                                        ]
                                    }
                                ],
                                "@odata.nextLink": ch_p2
                            }))
                        }
                    }
                }),
            )
            .route(
                "/v1.0/teams/t1/channels/ch1/messages/root-2/replies",
                get(move |RawQuery(q): RawQuery| {
                    let rep_p2 = rep_p2_c.clone();
                    async move {
                        let query = q.unwrap_or_default();
                        if query.contains("page=2") {
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "rep-2a",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:01:30Z",
                                        "from": { "user": { "id": "u2", "displayName": "B" } },
                                        "body": { "contentType": "text", "content": "first reply" }
                                    }
                                ]
                            }))
                        } else {
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "rep-2c",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:03:00Z",
                                        "from": { "user": { "id": "u2", "displayName": "B" } },
                                        "body": { "contentType": "text", "content": "third reply" }
                                    },
                                    {
                                        "id": "rep-2b",
                                        "messageType": "message",
                                        "createdDateTime": "2026-11-18T10:02:00Z",
                                        "from": { "user": { "id": "u1", "displayName": "A" } },
                                        "body": { "contentType": "text", "content": "second reply" }
                                    }
                                ],
                                "@odata.nextLink": rep_p2
                            }))
                        }
                    }
                }),
            );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = base;
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me-1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1");

        let (p1, next) = client
            .list_channel_messages_page("t1", "ch1", 50, None)
            .await
            .unwrap();
        let p1_ids: Vec<&str> = p1.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(p1_ids, vec!["root-2", "rep-2a", "root-3"]);
        assert_eq!(p1[1].parent_msg_id.as_deref(), Some("root-2"));
        assert_eq!(next.as_deref(), Some(ch_p2.as_str()));

        // limit = 2 root posts preserves inline expanded reply rep-2a as well
        let limited = client.list_channel_messages("t1", "ch1", 2).await.unwrap();
        let limited_ids: Vec<&str> = limited.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(limited_ids, vec!["root-2", "rep-2a", "root-3"]);

        // Multi-page thread replies
        let replies = client
            .list_thread_replies("t1", "ch1", "root-2", 100)
            .await
            .unwrap();
        let reply_ids: Vec<&str> = replies.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(reply_ids, vec!["rep-2a", "rep-2b", "rep-2c"]);
    }

    #[tokio::test]
    async fn fetch_thread_replies_walks_oldest_first_pages_to_return_latest() {
        use axum::{extract::Query, routing::get, Json, Router};
        use std::collections::HashMap;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let page2_url = format!("{base}/teams/t1/channels/ch1/messages/root-1/replies?page=2");
        let p2_clone = page2_url.clone();

        let router = Router::new().route(
            "/v1.0/teams/t1/channels/ch1/messages/root-1/replies",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let next = p2_clone.clone();
                async move {
                    let is_p2 = q.get("page").map(String::as_str) == Some("2");
                    let (start, end) = if is_p2 { (50usize, 60usize) } else { (0usize, 50usize) };
                    let vals: Vec<serde_json::Value> = (start..end)
                        .map(|i| {
                            serde_json::json!({
                                "id": format!("rep-{i:02}"),
                                "messageType": "message",
                                "createdDateTime": format!("2026-11-20T10:{:02}:00Z", i),
                                "from": { "user": { "id": "u1", "displayName": "Alice" } },
                                "body": { "contentType": "text", "content": format!("reply {i}") }
                            })
                        })
                        .collect();
                    if is_p2 {
                        Json(serde_json::json!({ "value": vals }))
                    } else {
                        Json(serde_json::json!({ "value": vals, "@odata.nextLink": next }))
                    }
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = base;
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me-1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1");

        let replies = client
            .list_thread_replies("t1", "ch1", "root-1", 50)
            .await
            .unwrap();
        assert_eq!(replies.len(), 50);
        assert_eq!(replies.first().unwrap().msg_id, "rep-10");
        assert_eq!(replies.last().unwrap().msg_id, "rep-59");
    }

    #[tokio::test]
    async fn list_joined_teams_with_channels_overlaps_pagination_and_fallback_preserving_order() {
        use axum::{http::StatusCode, routing::{get, post}, Json, Router};
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::sync::Barrier;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let t1_page2 = format!("{base}/teams/t1/channels?page=2");
        let t1_page2_for_batch = t1_page2.clone();

        // Both t1's pagination follow-up AND t2's fallback must arrive together to pass the barrier.
        let barrier = Arc::new(Barrier::new(2));
        let b1 = Arc::clone(&barrier);
        let b2 = Arc::clone(&barrier);

        let router = Router::new()
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            { "id": "t1", "displayName": "Team One" },
                            { "id": "t2", "displayName": "Team Two" },
                            { "id": "t3", "displayName": "Team Three" }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/$batch",
                post(move || {
                    let next = t1_page2_for_batch.clone();
                    async move {
                        Json(serde_json::json!({
                            "responses": [
                                {
                                    "id": "0",
                                    "status": 200,
                                    "body": {
                                        "value": [{ "id": "c1a", "displayName": "General" }],
                                        "@odata.nextLink": next
                                    }
                                },
                                {
                                    "id": "1",
                                    "status": 500,
                                    "body": { "error": { "code": "InternalServerError", "message": "retry direct" } }
                                },
                                {
                                    "id": "2",
                                    "status": 403,
                                    "body": { "error": { "code": "Forbidden", "message": "denied" } }
                                }
                            ]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/teams/t1/channels",
                get(move || {
                    let barrier = Arc::clone(&b1);
                    async move {
                        barrier.wait().await;
                        Json(serde_json::json!({
                            "value": [{ "id": "c1b", "displayName": "Random" }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/teams/t2/channels",
                get(move || {
                    let barrier = Arc::clone(&b2);
                    async move {
                        barrier.wait().await;
                        Json(serde_json::json!({
                            "value": [{ "id": "c2a", "displayName": "Design" }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/teams/t3/channels",
                get(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({
                            "error": { "code": "Forbidden", "message": "denied" }
                        })),
                    )
                }),
            );

        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = base;
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me-1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        let client = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1");

        let joined = tokio::time::timeout(
            Duration::from_secs(3),
            client.list_joined_teams_with_channels(),
        )
        .await
        .expect("per-team channel pagination and fallback must overlap concurrently")
        .unwrap();

        let team_ids: Vec<&str> = joined.teams.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(team_ids, vec!["t1", "t2", "t3"]);
        let t1_ch: Vec<&str> = joined.teams[0].channels.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(t1_ch, vec!["c1a", "c1b"]);
        let t2_ch: Vec<&str> = joined.teams[1].channels.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(t2_ch, vec!["c2a"]);
        assert!(joined.teams[2].channels.is_empty());
        assert_eq!(joined.channel_errors.len(), 1);
        assert_eq!(joined.channel_errors[0].team_id, "t3");
        assert_eq!(joined.channel_errors[0].status, Some(403));

        server.abort();
    }

    fn track5_channel_client(base: String) -> GraphClient {
        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = base;
        let tok = crate::auth::StoredTokens {
            access_token: "tok".into(),
            refresh_token: None,
            expires_at: 9_999_999_999,
            client_id: "c".into(),
            tenant_id: "t".into(),
            user_id: Some("me-1".into()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".into(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tok);
        GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1")
    }

    // Track 5: /allChannels lists channels shared into the team; 403/404 falls back to
    // /channels, while other errors are not masked by the fallback.
    #[tokio::test]
    async fn list_team_channels_uses_all_channels_with_fallback_on_403_404() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let own_hits = Arc::new(AtomicUsize::new(0));
        let own_hits_shared = Arc::clone(&own_hits);
        let own_hits_bad = Arc::clone(&own_hits);
        let router = Router::new()
            .route(
                "/v1.0/teams/t-all/allChannels",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            { "id": "ch-gen", "displayName": "General", "membershipType": "standard" },
                            { "id": "ch-in", "displayName": "Partner Room", "membershipType": "shared" }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/teams/t-all/channels",
                get(move || {
                    let hits = Arc::clone(&own_hits_shared);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/teams/t-forbidden/allChannels",
                get(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({
                            "error": { "code": "Forbidden", "message": "Shared channels are disabled" }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/teams/t-forbidden/channels",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "ch-own", "displayName": "General", "membershipType": "standard" }]
                    }))
                }),
            )
            // t-legacy has no /allChannels route: the mock answers 404.
            .route(
                "/v1.0/teams/t-legacy/channels",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "ch-legacy", "displayName": "Ops", "membershipType": "private" }]
                    }))
                }),
            )
            .route(
                "/v1.0/teams/t-bad/allChannels",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": { "code": "BadRequest", "message": "bad select" }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/teams/t-bad/channels",
                get(move || {
                    let hits = Arc::clone(&own_hits_bad);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let client = track5_channel_client(format!("http://{addr}/v1.0"));

        let ids = |chs: &[ChannelSummary]| -> Vec<(String, String)> {
            chs.iter()
                .map(|c| (c.id.clone(), c.membership_type.clone()))
                .collect()
        };
        let all = client.list_team_channels("t-all", "All").await.unwrap();
        assert_eq!(
            ids(&all),
            vec![
                ("ch-gen".to_string(), "standard".to_string()),
                ("ch-in".to_string(), "shared".to_string())
            ]
        );

        let forbidden = client
            .list_team_channels("t-forbidden", "Forbidden")
            .await
            .unwrap();
        assert_eq!(
            ids(&forbidden),
            vec![("ch-own".to_string(), "standard".to_string())]
        );

        let legacy = client.list_team_channels("t-legacy", "Legacy").await.unwrap();
        assert_eq!(
            ids(&legacy),
            vec![("ch-legacy".to_string(), "private".to_string())]
        );

        let bad = client.list_team_channels("t-bad", "Bad").await;
        let err = format!("{:#}", bad.expect_err("a 400 is not a missing endpoint"));
        assert_eq!(crate::graph::extract_graph_http_status(&err), Some(400));
        assert_eq!(
            own_hits.load(Ordering::SeqCst),
            0,
            "/channels is only asked when /allChannels answers 403/404"
        );

        server.abort();
    }

    // Track 5: the joined-teams $batch asks /allChannels; a 403/404 sub-response re-lists the
    // team's own channels instead of reporting the team as failed.
    #[tokio::test]
    async fn joined_teams_batch_requests_all_channels_and_falls_back_per_team() {
        use axum::{routing::{get, post}, Json, Router};
        use std::sync::{Arc, Mutex};

        let batch_urls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let batch_urls_c = Arc::clone(&batch_urls);
        let router = Router::new()
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            { "id": "t-all", "displayName": "All" },
                            { "id": "t-forbidden", "displayName": "Forbidden" },
                            { "id": "t-legacy", "displayName": "Legacy" }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/$batch",
                post(move |Json(body): Json<serde_json::Value>| {
                    let urls = Arc::clone(&batch_urls_c);
                    async move {
                        let reqs = body["requests"].as_array().cloned().unwrap_or_default();
                        let responses: Vec<serde_json::Value> = reqs
                            .iter()
                            .map(|req| {
                                let id = req["id"].as_str().unwrap_or("0");
                                let url = req["url"].as_str().unwrap_or("");
                                urls.lock().unwrap().push(url.to_string());
                                if url.contains("/teams/t-all/allChannels") {
                                    serde_json::json!({
                                        "id": id,
                                        "status": 200,
                                        "body": { "value": [
                                            { "id": "ch-gen", "displayName": "General", "membershipType": "standard" },
                                            { "id": "ch-in", "displayName": "Partner Room", "membershipType": "shared" }
                                        ] }
                                    })
                                } else if url.contains("/teams/t-forbidden/") {
                                    serde_json::json!({
                                        "id": id,
                                        "status": 403,
                                        "body": { "error": { "code": "Forbidden", "message": "denied" } }
                                    })
                                } else {
                                    serde_json::json!({
                                        "id": id,
                                        "status": 404,
                                        "body": { "error": { "code": "NotFound", "message": "missing" } }
                                    })
                                }
                            })
                            .collect();
                        Json(serde_json::json!({ "responses": responses }))
                    }
                }),
            )
            .route(
                "/v1.0/teams/t-forbidden/channels",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "ch-own", "displayName": "General" }]
                    }))
                }),
            )
            .route(
                "/v1.0/teams/t-legacy/channels",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "ch-legacy", "displayName": "Ops" }]
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let client = track5_channel_client(format!("http://{addr}/v1.0"));

        let joined = client.list_joined_teams_with_channels().await.unwrap();
        let urls = batch_urls.lock().unwrap().clone();
        assert_eq!(urls.len(), 3);
        assert!(
            urls.iter().all(|u| u.contains("/allChannels?")),
            "batch asks /allChannels: {urls:?}"
        );
        assert!(joined.channel_errors.is_empty(), "{:?}", joined.channel_errors);
        let channel_ids = |idx: usize| -> Vec<&str> {
            joined.teams[idx]
                .channels
                .iter()
                .map(|c| c.id.as_str())
                .collect()
        };
        assert_eq!(channel_ids(0), vec!["ch-gen", "ch-in"]);
        assert_eq!(joined.teams[0].channels[1].membership_type, "shared");
        assert_eq!(channel_ids(1), vec!["ch-own"]);
        assert_eq!(channel_ids(2), vec!["ch-legacy"]);

        server.abort();
    }
}
