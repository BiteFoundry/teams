//! Microsoft Graph v1.0 endpoints for Joined Teams, Channels, Delta Sync, Threads,
//! and unified Send / Reply / Edit / Delete / Reaction operations.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
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
    ChannelSummary, ConversationTarget, MemberInfo, MessageItem, OutgoingMessage, QuotedReply,
    TeamSummary, ThreadView,
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

impl GraphClient {
    pub async fn list_joined_teams_with_channels(&self) -> Result<Vec<TeamSummary>> {
        let mut next_url = Some("/me/joinedTeams?$select=id,displayName,description".to_string());
        let mut teams = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphTeam> = self.get_json(&url).await?;
            teams.extend(page.value);
            next_url = page.next_link;
        }

        if teams.is_empty() {
            return Ok(Vec::new());
        }

        let requests: Vec<BatchSubRequest> = teams
            .iter()
            .enumerate()
            .map(|(idx, team)| BatchSubRequest {
                id: idx.to_string(),
                method: "GET".to_string(),
                url: format!(
                    "/teams/{}/channels?$select=id,displayName,description,membershipType,webUrl",
                    urlencoding::encode(&team.id)
                ),
                body: None,
                headers: None,
            })
            .collect();

        let responses = self.batch(&requests).await?;
        let mut channels_by_idx: HashMap<usize, Vec<GraphChannel>> = HashMap::new();
        for resp in responses {
            let Ok(idx) = resp.id.parse::<usize>() else {
                continue;
            };
            if !(200..300).contains(&resp.status) {
                continue;
            }
            if let Ok(page) = serde_json::from_value::<GraphListPage<GraphChannel>>(resp.body) {
                channels_by_idx.insert(idx, page.value);
            }
        }

        let summaries = teams
            .into_iter()
            .enumerate()
            .map(|(idx, team)| {
                let team_name = team
                    .display_name
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| team.id.clone());
                let raw_channels = channels_by_idx.remove(&idx).unwrap_or_default();
                let channels = raw_channels
                    .into_iter()
                    .map(|ch| ch.into_summary(&team.id, &team_name))
                    .collect();
                TeamSummary {
                    id: team.id,
                    display_name: team_name,
                    description: team.description.filter(|s| !s.trim().is_empty()),
                    channels,
                }
            })
            .collect();

        Ok(summaries)
    }

    pub async fn list_team_channels(
        &self,
        team_id: &str,
        team_name: &str,
    ) -> Result<Vec<ChannelSummary>> {
        let mut next_url = Some(format!(
            "/teams/{}/channels?$select=id,displayName,description,membershipType,webUrl",
            urlencoding::encode(team_id)
        ));
        let mut channels = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphChannel> = self.get_json(&url).await?;
            channels.extend(
                page.value
                    .into_iter()
                    .map(|ch| ch.into_summary(team_id, team_name)),
            );
            next_url = page.next_link;
        }
        Ok(channels)
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
        if limit == 0 {
            return Ok(Vec::new());
        }
        let my_id = self.resolve_my_user_id().await;
        let conv_key = ConversationTarget::Channel {
            team_id: team_id.to_string(),
            channel_id: channel_id.to_string(),
        }
        .conv_key();

        let top = limit.clamp(1, 50);
        let mut next_url = Some(format!(
            "/teams/{}/channels/{}/messages/{}/replies?$top={top}",
            urlencoding::encode(team_id),
            urlencoding::encode(channel_id),
            urlencoding::encode(root_msg_id)
        ));
        let mut replies = Vec::new();
        let mut seen_ids = HashSet::new();

        while let Some(url) = next_url {
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
            if replies.len() >= limit {
                break;
            }
            next_url = page
                .next_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
        }

        sort_messages_chronologically(&mut replies);
        truncate_to_newest(&mut replies, limit);
        Ok(replies)
    }

    pub async fn get_thread(
        &self,
        team_id: &str,
        channel_id: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<ThreadView> {
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
        let (root_raw, replies) = tokio::try_join!(
            self.get_json::<GraphChatMessage>(&root_path),
            self.list_thread_replies(team_id, channel_id, root_msg_id, limit)
        )?;

        let mut root = normalize_chat_message(&conv_key, None, my_id.as_deref(), root_raw);
        root.reply_count = root.reply_count.max(replies.len());

        Ok(ThreadView {
            conv_key,
            root,
            replies,
        })
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
                    if let Ok(orig) = self.get_json::<GraphChatMessage>(&msg_path).await {
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
                    } else {
                        QuotedReply {
                            msg_id: root_or_target_msg_id.to_string(),
                            author_name: String::new(),
                            preview_plain: String::new(),
                        }
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
}
