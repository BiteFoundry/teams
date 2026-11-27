//! Microsoft Graph v1.0 endpoints for User Profile, User Search, and 1:1 / Group / Meeting Chats.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use super::{
    client::{
        compare_rfc3339_gt, is_pure_system_noise, normalize_chat_message,
        sort_messages_chronologically, truncate_to_newest, BatchSubRequest, BatchSubResponse,
        GraphChatMessage,
        GraphIdentitySet, GraphItemBody, GraphListPage,
    },
    GraphClient,
};
use crate::models::{
    ConversationKind, ConversationSummary, ConversationTarget, MemberInfo, MessageItem, UserProfile,
};

#[derive(Debug, Deserialize)]
struct GraphUser {
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    mail: Option<String>,
    #[serde(rename = "userPrincipalName", default)]
    user_principal_name: Option<String>,
}

impl From<GraphUser> for UserProfile {
    fn from(u: GraphUser) -> Self {
        let fallback_name = u
            .user_principal_name
            .clone()
            .or_else(|| u.mail.clone())
            .unwrap_or_else(|| u.id.clone());
        Self {
            id: u.id,
            display_name: u
                .display_name
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(fallback_name),
            mail: u.mail,
            user_principal_name: u.user_principal_name,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct GraphConversationMember {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(rename = "userId", default)]
    pub user_id: Option<String>,
    #[serde(rename = "displayName", default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}

impl From<GraphConversationMember> for MemberInfo {
    fn from(m: GraphConversationMember) -> Self {
        let uid = m.user_id.unwrap_or_default();
        let name = m
            .display_name
            .filter(|s| !s.trim().is_empty())
            .or_else(|| m.email.clone())
            .unwrap_or_else(|| uid.clone());
        Self {
            user_id: uid,
            member_id: m.id,
            display_name: name,
            email: m.email,
            roles: m.roles,
            presence: None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct GraphChatMessagePreview {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "isDeleted", default)]
    is_deleted: Option<bool>,
    #[serde(default)]
    from: Option<GraphIdentitySet>,
    #[serde(default)]
    body: Option<GraphItemBody>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphChatViewpoint {
    #[serde(rename = "lastMessageReadDateTime", default)]
    last_message_read_date_time: Option<String>,
}

fn deserialize_explicit_opt_string<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Default, Deserialize)]
struct GraphChat {
    id: String,
    #[serde(rename = "chatType", default)]
    chat_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_explicit_opt_string")]
    topic: Option<Option<String>>,
    #[serde(rename = "lastUpdatedDateTime", default)]
    last_updated_date_time: Option<String>,
    #[serde(rename = "webUrl", default)]
    web_url: Option<String>,
    #[serde(rename = "lastMessagePreview", default)]
    last_message_preview: Option<GraphChatMessagePreview>,
    #[serde(default)]
    members: Vec<GraphConversationMember>,
    #[serde(default)]
    viewpoint: Option<GraphChatViewpoint>,
}

#[derive(Debug, Default)]
pub(crate) struct ChatsListResult {
    pub chats: Vec<ConversationSummary>,
    pub next_link: Option<String>,
    pub eof: bool,
    pub explicit_null_topic_keys: HashSet<String>,
}

impl GraphClient {
    pub async fn get_me(&self) -> Result<UserProfile> {
        let user: GraphUser = self
            .get_json("/me?$select=id,displayName,mail,userPrincipalName")
            .await?;
        if !user.id.trim().is_empty() {
            let _ = self.cached_my_user_id.set(user.id.clone());
        }
        Ok(user.into())
    }

    pub async fn search_users(&self, query: &str, limit: usize) -> Result<Vec<UserProfile>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let q = query.trim();
        if q.is_empty() {
            let page: GraphListPage<GraphUser> = self
                .get_json(&format!(
                    "/users?$top={limit}&$select=id,displayName,mail,userPrincipalName"
                ))
                .await?;
            return Ok(page.value.into_iter().map(Into::into).collect());
        }

        if !self
            .users_filter_unsupported
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            let escaped = q.replace('\'', "''");
            let filter =
                format!("startswith(displayName,'{escaped}') or startswith(mail,'{escaped}')");
            let path = format!(
                "/users?$filter={}&$top={limit}&$select=id,displayName,mail,userPrincipalName",
                urlencoding::encode(&filter)
            );
            match self.get_json::<GraphListPage<GraphUser>>(&path).await {
                Ok(page) => return Ok(page.value.into_iter().map(Into::into).collect()),
                Err(err) if is_users_filter_unsupported(&err) => {
                    self.users_filter_unsupported
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                Err(err) => return Err(err),
            }
        }

        // Fallback when `$filter` is unsupported or restricted in the tenant
        let q_lower = q.to_lowercase();
        let fetch_top = limit.max(100);
        let page: GraphListPage<GraphUser> = self
            .get_json(&format!(
                "/users?$top={fetch_top}&$select=id,displayName,mail,userPrincipalName"
            ))
            .await?;
        let mut matches: Vec<UserProfile> = page
            .value
            .into_iter()
            .map(UserProfile::from)
            .filter(|u| {
                u.display_name.to_lowercase().contains(&q_lower)
                    || u.mail
                        .as_deref()
                        .map(|m| m.to_lowercase().contains(&q_lower))
                        .unwrap_or(false)
                    || u.user_principal_name
                        .as_deref()
                        .map(|p| p.to_lowercase().contains(&q_lower))
                        .unwrap_or(false)
            })
            .collect();
        matches.truncate(limit);
        Ok(matches)
    }

    /// Fetches a single page of `/me/chats` (preferring `$orderby=lastMessagePreview/createdDateTime desc`
    /// with transparent fallback when `$orderby` is unsupported) and returns `(chats, next_link)`.
    pub async fn list_chats_page(
        &self,
        top: usize,
        next_link: Option<&str>,
    ) -> Result<(Vec<ConversationSummary>, Option<String>)> {
        let res = self.list_chats_page_detailed(top, next_link).await?;
        Ok((res.chats, res.next_link))
    }

    pub(crate) async fn list_chats_page_detailed(
        &self,
        top: usize,
        next_link: Option<&str>,
    ) -> Result<ChatsListResult> {
        let my_id = self.resolve_my_user_id().await;
        let page: GraphListPage<GraphChat> = match next_link.map(str::trim).filter(|s| !s.is_empty())
        {
            Some(url) => self.get_json(url).await?,
            None => {
                let page_top = top.clamp(1, 50);
                let fallback_url = format!(
                    "/me/chats?$expand=members,lastMessagePreview&$top={page_top}"
                );
                if self
                    .chats_orderby_unsupported
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    self.get_json(&fallback_url).await?
                } else {
                    let ordered_url = format!(
                        "/me/chats?$expand=members,lastMessagePreview&$orderby=lastMessagePreview/createdDateTime%20desc&$top={page_top}"
                    );
                    match self.get_json(&ordered_url).await {
                        Ok(p) => p,
                        Err(err) if is_chats_orderby_unsupported(&err) => {
                            self.chats_orderby_unsupported
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            self.get_json(&fallback_url).await?
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
        };

        let mut chats = Vec::with_capacity(page.value.len());
        let mut explicit_null_topic_keys = HashSet::new();
        for chat in page.value {
            let (summary, topic_present) = map_graph_chat_detailed(chat, my_id.as_deref());
            if topic_present && summary.topic.is_none() {
                explicit_null_topic_keys.insert(summary.conv_key.clone());
            }
            chats.push(summary);
        }
        let next = page
            .next_link
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let eof = next.is_none();
        Ok(ChatsListResult {
            chats,
            next_link: next,
            eof,
            explicit_null_topic_keys,
        })
    }

    pub async fn list_chats(&self, limit: usize) -> Result<Vec<ConversationSummary>> {
        Ok(self.list_chats_with_cursor(limit).await?.chats)
    }

    pub(crate) async fn list_chats_with_cursor(&self, limit: usize) -> Result<ChatsListResult> {
        if limit == 0 {
            return Ok(ChatsListResult::default());
        }

        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut explicit_null_topic_keys = HashSet::new();
        let mut next_link: Option<String> = None;
        let mut truncated_page = false;

        loop {
            let remaining = limit.saturating_sub(out.len());
            if remaining == 0 {
                break;
            }
            // Later pages inherit the first page's `$top` through the nextLink, so a page can
            // hold more rows than `remaining`. Remember the link that fetched it.
            let page_link_used = next_link.clone();
            let page = self
                .list_chats_page_detailed(remaining.min(50), next_link.as_deref())
                .await?;
            for chat in page.chats {
                if seen.insert(chat.conv_key.clone()) {
                    if out.len() >= limit {
                        truncated_page = true;
                        break;
                    }
                    if page.explicit_null_topic_keys.contains(&chat.conv_key) {
                        explicit_null_topic_keys.insert(chat.conv_key.clone());
                    }
                    out.push(chat);
                }
            }
            if truncated_page {
                // Rows of this page were left out: hand back the link to this same page, not
                // to the next one, so a caller following the cursor re-reads it instead of
                // skipping the rest (already-seen rows de-duplicate by `conv_key`). `None`
                // here (the first page over-returned) means "start over", never EOF.
                next_link = page_link_used;
                break;
            }
            next_link = page.next_link;
            if out.len() >= limit || next_link.is_none() {
                break;
            }
        }

        out.truncate(limit);
        let eof = next_link.is_none() && !truncated_page;
        Ok(ChatsListResult {
            chats: out,
            next_link,
            eof,
            explicit_null_topic_keys,
        })
    }

    /// Creates a `oneOnOne` (1 peer user ID/email) or `group` (2+ peers, optional topic) chat.
    pub async fn create_chat(
        &self,
        member_ids_or_emails: &[String],
        topic: Option<&str>,
    ) -> Result<ConversationSummary> {
        let peers: Vec<String> = member_ids_or_emails
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if peers.is_empty() {
            bail!("At least one recipient must be specified.");
        }

        let my_id = self.require_my_user_id().await?;
        let topic_clean = topic.map(str::trim).filter(|s| !s.is_empty());
        let is_one_on_one = peers.len() == 1 && topic_clean.is_none();
        let chat_type = if is_one_on_one { "oneOnOne" } else { "group" };

        let mut members_json = vec![serde_json::json!({
            "@odata.type": "#microsoft.graph.aadUserConversationMember",
            "roles": ["owner"],
            "user@odata.bind": user_bind_url(&my_id)
        })];

        for peer in &peers {
            if peer == &my_id && !is_one_on_one {
                continue;
            }
            members_json.push(serde_json::json!({
                "@odata.type": "#microsoft.graph.aadUserConversationMember",
                "roles": ["owner"],
                "user@odata.bind": user_bind_url(peer)
            }));
        }

        let mut body = serde_json::json!({
            "chatType": chat_type,
            "members": members_json,
        });
        if let (false, Some(t)) = (is_one_on_one, topic_clean) {
            body["topic"] = serde_json::Value::String(t.to_string());
        }

        let created: GraphChat = self.post_json("/chats", &body).await?;
        Ok(map_graph_chat(created, Some(&my_id)))
    }

    pub(crate) async fn update_group_chat_topic(
        &self,
        chat_id: &str,
        topic: &str,
    ) -> Result<ConversationSummary> {
        let path = format!("/chats/{}", urlencoding::encode(chat_id));
        let current: GraphChat = self.get_json(&path).await?;
        if current.id != chat_id || current.chat_type.as_deref() != Some("group") {
            bail!("Chat topic changes require an existing group chat.");
        }

        let payload = serde_json::json!({ "topic": topic });
        let updated: GraphChat = self
            .patch_json_with_headers(&path, &payload, &[])
            .await?
            .json()
            .await
            .context("Graph accepted the topic update, but its response could not be read")?;
        if updated.id != chat_id || updated.topic.as_ref().and_then(|o| o.as_deref()) != Some(topic) {
            bail!("Graph accepted the topic update, but its response did not confirm the requested chat ID and topic.");
        }
        Ok(map_graph_chat(updated, self.my_user_id()))
    }

    pub async fn list_chat_members(&self, chat_id: &str) -> Result<Vec<MemberInfo>> {
        let mut next_url = Some(format!("/chats/{}/members", urlencoding::encode(chat_id)));
        let mut members = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphConversationMember> = self.get_json(&url).await?;
            members.extend(page.value.into_iter().map(MemberInfo::from));
            next_url = page.next_link;
        }
        Ok(members)
    }

    /// Fetches a single page of chat messages (oldest-first within the batch), automatically advancing
    /// up to 5 consecutive pages if a non-empty raw Graph page contains only system noise.
    pub async fn list_chat_messages_page(
        &self,
        chat_id: &str,
        top: usize,
        next_link: Option<&str>,
    ) -> Result<(Vec<MessageItem>, Option<String>)> {
        let my_id = self.resolve_my_user_id().await;
        let conv_key = ConversationTarget::Chat {
            chat_id: chat_id.to_string(),
        }
        .conv_key();

        let page_top = top.clamp(1, 50);
        let mut url = next_link
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                format!(
                    "/chats/{}/messages?$top={page_top}",
                    urlencoding::encode(chat_id)
                )
            });

        let mut msgs = Vec::new();
        let mut current_next_link;
        let mut empty_noise_pages = 0usize;

        loop {
            let page: GraphListPage<GraphChatMessage> = self.get_json(&url).await?;
            let raw_non_empty = !page.value.is_empty();
            for raw in page.value {
                let normalized = normalize_chat_message(&conv_key, None, my_id.as_deref(), raw);
                if !is_pure_system_noise(&normalized) {
                    msgs.push(normalized);
                }
            }
            current_next_link = page
                .next_link
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());

            if msgs.is_empty() && raw_non_empty && empty_noise_pages < 5 {
                if let Some(next_url) = current_next_link.take() {
                    empty_noise_pages += 1;
                    url = next_url;
                    continue;
                }
            }
            break;
        }

        sort_messages_chronologically(&mut msgs);
        Ok((msgs, current_next_link))
    }

    pub async fn list_chat_messages(&self, chat_id: &str, limit: usize) -> Result<Vec<MessageItem>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut items = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut next_link: Option<String> = None;

        loop {
            let (page_msgs, next) = self
                .list_chat_messages_page(chat_id, limit.min(50), next_link.as_deref())
                .await?;
            for msg in page_msgs {
                if seen_ids.insert(msg.msg_id.clone()) {
                    items.push(msg);
                }
            }
            if items.len() >= limit || next.is_none() {
                break;
            }
            next_link = next;
        }

        sort_messages_chronologically(&mut items);
        truncate_to_newest(&mut items, limit);
        Ok(items)
    }

    pub async fn mark_chat_read(&self, chat_id: &str) -> Result<()> {
        let me_id = self.require_my_user_id().await?;
        let path = format!(
            "/chats/{}/markChatReadForUser",
            urlencoding::encode(chat_id)
        );
        let body = serde_json::json!({
            "user": { "id": me_id }
        });
        self.post_json_empty(&path, &body).await
    }

    /// Fetches multiple chat message lists in a single `POST /$batch` call (chunked by 20).
    ///
    /// The messages are `None` for a chat whose sub-response is missing, not 2xx (throttled,
    /// forbidden, ...) or unparseable, so the caller can tell "no messages" from "not fetched".
    pub async fn batch_fetch_chat_messages(
        &self,
        chat_ids: &[String],
        limit_per_chat: usize,
    ) -> Result<Vec<(String, Option<Vec<MessageItem>>)>> {
        if chat_ids.is_empty() || limit_per_chat == 0 {
            return Ok(Vec::new());
        }
        let my_id = self.resolve_my_user_id().await;
        let top = limit_per_chat.min(50);

        let requests: Vec<BatchSubRequest> = chat_ids
            .iter()
            .enumerate()
            .map(|(idx, chat_id)| BatchSubRequest {
                id: idx.to_string(),
                method: "GET".to_string(),
                url: format!("/chats/{}/messages?$top={top}", urlencoding::encode(chat_id)),
                body: None,
                headers: None,
            })
            .collect();

        let responses = self.batch(&requests).await?;
        Ok(collect_batch_chat_messages(chat_ids, responses, my_id.as_deref(), limit_per_chat))
    }
}

/// Pairs each requested chat with its `$batch` sub-response, in request order.
fn collect_batch_chat_messages(
    chat_ids: &[String],
    responses: Vec<BatchSubResponse>,
    my_id: Option<&str>,
    limit_per_chat: usize,
) -> Vec<(String, Option<Vec<MessageItem>>)> {
    let mut by_idx: std::collections::HashMap<usize, Vec<MessageItem>> =
        std::collections::HashMap::new();

    for resp in responses {
        let Ok(idx) = resp.id.parse::<usize>() else {
            continue;
        };
        let Some(chat_id) = chat_ids.get(idx) else {
            continue;
        };
        if !(200..300).contains(&resp.status) {
            continue;
        }
        let Ok(page) = serde_json::from_value::<GraphListPage<GraphChatMessage>>(resp.body) else {
            continue;
        };
        let conv_key = ConversationTarget::Chat {
            chat_id: chat_id.clone(),
        }
        .conv_key();
        let mut items: Vec<MessageItem> = page
            .value
            .into_iter()
            .map(|raw| normalize_chat_message(&conv_key, None, my_id, raw))
            .filter(|m| !is_pure_system_noise(m))
            .collect();
        sort_messages_chronologically(&mut items);
        truncate_to_newest(&mut items, limit_per_chat);
        by_idx.insert(idx, items);
    }

    chat_ids
        .iter()
        .enumerate()
        .map(|(idx, cid)| (cid.clone(), by_idx.remove(&idx)))
        .collect()
}

fn is_chats_orderby_unsupported(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    let detail = msg
        .strip_prefix(crate::graph::GRAPH_API_ERROR_PREFIX)
        .and_then(|rest| rest.split_once("): ").map(|(_, d)| d))
        .unwrap_or(&msg);
    let lower = detail.to_ascii_lowercase();
    detail.contains("400")
        || lower.contains("bad request")
        || lower.contains("orderby")
        || detail.contains("501")
}

fn map_graph_chat(chat: GraphChat, my_user_id: Option<&str>) -> ConversationSummary {
    map_graph_chat_detailed(chat, my_user_id).0
}

fn map_graph_chat_detailed(chat: GraphChat, my_user_id: Option<&str>) -> (ConversationSummary, bool) {
    let topic_present = chat.topic.is_some();
    let kind = match chat.chat_type.as_deref() {
        Some("oneOnOne") => ConversationKind::OneOnOne,
        Some("meeting") => ConversationKind::Meeting,
        _ => ConversationKind::Group,
    };

    let target = ConversationTarget::Chat {
        chat_id: chat.id.clone(),
    };
    let conv_key = target.conv_key();

    let non_self_members: Vec<&GraphConversationMember> = chat
        .members
        .iter()
        .filter(|m| match (m.user_id.as_deref(), my_user_id) {
            (Some(uid), Some(me)) => uid != me,
            _ => true,
        })
        .collect();

    let peer_user_id = if kind == ConversationKind::OneOnOne {
        non_self_members
            .first()
            .and_then(|m| m.user_id.clone())
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                chat.members
                    .first()
                    .and_then(|m| m.user_id.clone())
                    .filter(|s| !s.trim().is_empty())
            })
    } else {
        None
    };

    let topic = chat
        .topic
        .flatten()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    let title = if let Some(ref t) = topic {
        t.clone()
    } else if kind == ConversationKind::OneOnOne {
        non_self_members
            .first()
            .and_then(|m| m.display_name.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                chat.members
                    .first()
                    .and_then(|m| m.display_name.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or("Direct Message")
            .to_string()
    } else {
        let names: Vec<String> = non_self_members
            .iter()
            .filter_map(|m| m.display_name.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .take(4)
            .map(str::to_string)
            .collect();
        if names.is_empty() {
            if kind == ConversationKind::Meeting {
                "Meeting Chat".to_string()
            } else {
                "Group Chat".to_string()
            }
        } else {
            names.join(", ")
        }
    };

    let last_read_at = chat
        .viewpoint
        .and_then(|v| v.last_message_read_date_time)
        .filter(|s| !s.trim().is_empty());

    let (last_msg_id, last_msg_author, last_msg_preview, last_msg_created, last_msg_sender_id) =
        if let Some(preview) = chat.last_message_preview {
            let preview_text = if preview.is_deleted == Some(true) {
                Some("(deleted message)".to_string())
            } else {
                let raw_html = preview
                    .body
                    .as_ref()
                    .and_then(|b| b.content.as_deref())
                    .unwrap_or("");
                let plain = crate::parser::html_and_cards_to_plain(raw_html, &[]);
                let trimmed = plain.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            };
            let author = preview
                .from
                .as_ref()
                .and_then(|f| f.primary_name())
                .map(str::to_string);
            let sender_id = preview
                .from
                .as_ref()
                .and_then(|f| f.primary_id())
                .map(str::to_string);
            (
                preview.id,
                author,
                preview_text,
                preview.created_date_time,
                sender_id,
            )
        } else {
            (None, None, None, None, None)
        };

    let is_unread_timestamp =
        compare_rfc3339_gt(last_msg_created.as_deref(), last_read_at.as_deref());
    let sent_by_me = match (last_msg_sender_id.as_deref(), my_user_id) {
        (Some(sid), Some(me)) => sid == me,
        _ => false,
    };
    let unread_count = if is_unread_timestamp && !sent_by_me {
        1
    } else {
        0
    };

    let last_updated_at = match (
        last_msg_created.as_deref(),
        chat.last_updated_date_time.as_deref(),
    ) {
        (Some(msg_ts), Some(chat_ts)) => {
            if compare_rfc3339_gt(Some(chat_ts), Some(msg_ts)) {
                chat.last_updated_date_time
            } else {
                last_msg_created
            }
        }
        (Some(_), None) => last_msg_created,
        (None, Some(_)) => chat.last_updated_date_time,
        (None, None) => None,
    };

    (
        ConversationSummary {
            conv_key,
            target,
            kind,
            team_id: None,
            team_name: None,
            title,
            topic,
            unread_count,
            last_read_at,
            last_msg_id,
            last_msg_author,
            last_msg_preview,
            last_updated_at,
            pinned: false,
            peer_user_id,
            web_url: chat.web_url,
        },
        topic_present,
    )
}

fn is_users_filter_unsupported(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    if matches!(super::extract_graph_http_status(&msg), Some(401 | 403)) {
        return false;
    }
    if matches!(super::extract_graph_http_status(&msg), Some(400 | 501)) {
        return true;
    }
    let lower = msg.to_ascii_lowercase();
    lower.contains("request_unsupportedquery")
        || lower.contains("unsupported")
        || lower.contains("not supported")
        || lower.contains("filter")
}

/// `user@odata.bind` URL for a chat member given as an object id or a UPN/email.
///
/// The key is an OData string literal inside a URL: a single quote is doubled (`'` → `''`)
/// and the characters that would end or re-shape the URL are percent-encoded, so
/// `o'brien@contoso.com` and guest UPNs (`user_ext.com#EXT#@tenant.onmicrosoft.com`) bind
/// correctly. GUIDs and ordinary addresses come out unchanged.
pub(crate) fn user_bind_url(id_or_upn: &str) -> String {
    let mut key = String::with_capacity(id_or_upn.len());
    for c in id_or_upn.chars() {
        match c {
            '\'' => key.push_str("''"),
            '%' => key.push_str("%25"),
            '#' => key.push_str("%23"),
            '?' => key.push_str("%3F"),
            '/' => key.push_str("%2F"),
            ' ' => key.push_str("%20"),
            other => key.push(other),
        }
    }
    format!("https://graph.microsoft.com/v1.0/users('{key}')")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_bind_url_escapes_odata_key_and_keeps_plain_ids() {
        // GUIDs and ordinary addresses are byte-identical to the previous format!() output.
        for plain in ["0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0", "alice@contoso.com", "first.last_x-y@sub.contoso.com"] {
            assert_eq!(
                user_bind_url(plain),
                format!("https://graph.microsoft.com/v1.0/users('{plain}')")
            );
        }
        assert_eq!(
            user_bind_url("o'brien@contoso.com"),
            "https://graph.microsoft.com/v1.0/users('o''brien@contoso.com')"
        );
        assert_eq!(
            user_bind_url("user_ext.com#EXT#@tenant.onmicrosoft.com"),
            "https://graph.microsoft.com/v1.0/users('user_ext.com%23EXT%23@tenant.onmicrosoft.com')"
        );
        assert_eq!(
            user_bind_url("a b/c?d%e"),
            "https://graph.microsoft.com/v1.0/users('a%20b%2Fc%3Fd%25e')"
        );
    }

    #[test]
    fn test_chat_title_and_unread_derivation_one_on_one() {
        let raw: GraphChat = serde_json::from_value(serde_json::json!({
            "id": "19:abc@unq.gbl.spaces",
            "chatType": "oneOnOne",
            "members": [
                { "userId": "me-1", "displayName": "Me" },
                { "userId": "peer-2", "displayName": "Alice" }
            ],
            "viewpoint": {
                "lastMessageReadDateTime": "2026-11-15T05:00:00Z"
            },
            "lastMessagePreview": {
                "id": "m100",
                "createdDateTime": "2026-11-15T06:00:00Z",
                "from": { "user": { "id": "peer-2", "displayName": "Alice" } },
                "body": { "contentType": "html", "content": "Hello" }
            }
        }))
        .unwrap();

        let summary = map_graph_chat(raw, Some("me-1"));
        assert_eq!(summary.kind, ConversationKind::OneOnOne);
        assert_eq!(summary.title, "Alice");
        assert_eq!(summary.peer_user_id.as_deref(), Some("peer-2"));
        assert_eq!(summary.unread_count, 1);
        assert_eq!(summary.last_msg_author.as_deref(), Some("Alice"));
    }

    #[test]
    fn test_chat_title_group_without_topic_and_read_own_msg() {
        let raw: GraphChat = serde_json::from_value(serde_json::json!({
            "id": "19:grp@thread.v2",
            "chatType": "group",
            "topic": "",
            "members": [
                { "userId": "me-1", "displayName": "Me" },
                { "userId": "u2", "displayName": "Bob" },
                { "userId": "u3", "displayName": "Carol" },
                { "userId": "u4", "displayName": "Dave" },
                { "userId": "u5", "displayName": "Eve" },
                { "userId": "u6", "displayName": "Frank" }
            ],
            "viewpoint": {
                "lastMessageReadDateTime": "2026-11-15T05:00:00Z"
            },
            "lastMessagePreview": {
                "id": "m200",
                "createdDateTime": "2026-11-15T06:00:00Z",
                "from": { "user": { "id": "me-1", "displayName": "Me" } },
                "body": { "contentType": "text", "content": "Message I sent" }
            }
        }))
        .unwrap();

        let summary = map_graph_chat(raw, Some("me-1"));
        assert_eq!(summary.kind, ConversationKind::Group);
        assert_eq!(summary.title, "Bob, Carol, Dave, Eve");
        assert_eq!(summary.peer_user_id, None);
        // Sent by me -> unread_count must be 0
        assert_eq!(summary.unread_count, 0);
    }

    #[test]
    fn test_chat_title_group_with_topic() {
        let raw: GraphChat = serde_json::from_value(serde_json::json!({
            "id": "19:grp2@thread.v2",
            "chatType": "group",
            "topic": "Backend Sprint",
            "members": [
                { "userId": "me-1", "displayName": "Me" },
                { "userId": "u2", "displayName": "Bob" }
            ]
        }))
        .unwrap();

        let summary = map_graph_chat(raw, Some("me-1"));
        assert_eq!(summary.title, "Backend Sprint");
        assert_eq!(summary.topic.as_deref(), Some("Backend Sprint"));
        assert_eq!(summary.unread_count, 0);
    }

    #[tokio::test]
    async fn test_list_chats_page_orderby_fallback_and_pagination() {
        use axum::{extract::RawQuery, http::StatusCode, routing::get, Json, Router};
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };

        let ordered_attempts = Arc::new(AtomicUsize::new(0));
        let ordered_clone = ordered_attempts.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let page2_url = format!("{base}/me/chats?page=2");
        let page2_url_clone = page2_url.clone();

        let app = Router::new().route(
            "/v1.0/me/chats",
            get(move |RawQuery(q): RawQuery| {
                let ordered_attempts = ordered_clone.clone();
                let page2_url = page2_url_clone.clone();
                async move {
                    let query = q.unwrap_or_default();
                    if query.contains("orderby") {
                        let n = ordered_attempts.fetch_add(1, Ordering::SeqCst);
                        return match n {
                            0 | 1 => (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({
                                    "error": { "code": "BadRequest", "message": "Invalid $orderby" }
                                })),
                            ),
                            2 => (
                                StatusCode::NOT_IMPLEMENTED,
                                Json(serde_json::json!({
                                    "error": { "code": "NotImplemented", "message": "OrderBy unsupported" }
                                })),
                            ),
                            _ => (
                                StatusCode::FORBIDDEN,
                                Json(serde_json::json!({
                                    "error": { "code": "Forbidden", "message": "Missing Chat.Read" }
                                })),
                            ),
                        };
                    }
                    if query.contains("page=2") {
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    { "id": "chat-2", "chatType": "group", "topic": "Dup" },
                                    { "id": "chat-3", "chatType": "group", "topic": "Three" }
                                ]
                            })),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "value": [
                                { "id": "chat-1", "chatType": "group", "topic": "One" },
                                { "id": "chat-2", "chatType": "group", "topic": "Two" }
                            ],
                            "@odata.nextLink": page2_url
                        })),
                    )
                }
            }),
        );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = crate::config::AppConfig {
            graph_base_url: base,
            ..Default::default()
        };
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
        let client = GraphClient::new(cfg.clone(), auth.clone())
            .unwrap()
            .with_my_user_id("me-1");

        // Attempt 0: 400 Bad Request triggers fallback and memoizes unsupported $orderby
        let (p1, next) = client.list_chats_page(2, None).await.unwrap();
        assert_eq!(ordered_attempts.load(Ordering::SeqCst), 1);
        assert_eq!(p1.len(), 2);
        assert_eq!(next.as_deref(), Some(page2_url.as_str()));

        // Subsequent call on the same client skips $orderby completely (ordered_attempts stays 1)
        let all = client.list_chats(10).await.unwrap();
        assert_eq!(ordered_attempts.load(Ordering::SeqCst), 1);
        let keys: Vec<&str> = all.iter().map(|c| c.conv_key.as_str()).collect();
        assert_eq!(keys, vec!["chat:chat-1", "chat:chat-2", "chat:chat-3"]);
        assert_eq!(all[0].title, "One");
        assert_eq!(all[1].title, "Two");
        assert_eq!(all[2].title, "Three");

        // Fresh client #2: 400 (n=1) -> fallback; fresh client #3: 501 Not Implemented (n=2) -> fallback
        let client_400 = GraphClient::new(cfg.clone(), auth.clone())
            .unwrap()
            .with_my_user_id("me-1");
        let (p1_400, _) = client_400.list_chats_page(2, None).await.unwrap();
        assert_eq!(p1_400.len(), 2);

        let client_501 = GraphClient::new(cfg.clone(), auth.clone())
            .unwrap()
            .with_my_user_id("me-1");
        let (p1_501, _) = client_501.list_chats_page(2, None).await.unwrap();
        assert_eq!(p1_501.len(), 2);

        // Fresh client #4: 403 Forbidden (n=3) must NOT trigger fallback
        let client_403 = GraphClient::new(cfg, auth)
            .unwrap()
            .with_my_user_id("me-1");
        let err = client_403.list_chats_page(2, None).await.unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn test_list_chat_messages_page_skips_system_noise_and_paginates() {
        use axum::{extract::RawQuery, routing::get, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let p2_url = format!("{base}/chats/c1/messages?page=2");
        let p3_url = format!("{base}/chats/c1/messages?page=3");
        let p4_url = format!("{base}/chats/c1/messages?page=4");
        let (p2_c, p3_c, p4_c) = (p2_url.clone(), p3_url.clone(), p4_url.clone());

        let app = Router::new().route(
            "/v1.0/chats/c1/messages",
            get(move |RawQuery(q): RawQuery| {
                let (p2_url, p3_url, p4_url) = (p2_c.clone(), p3_c.clone(), p4_c.clone());
                async move {
                    let query = q.unwrap_or_default();
                    if query.contains("page=2") {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "sys-2",
                                    "messageType": "systemEventMessage",
                                    "createdDateTime": "2026-11-18T10:04:00Z",
                                    "body": { "contentType": "html", "content": "<systemEventMessage/>" }
                                }
                            ],
                            "@odata.nextLink": p3_url
                        }))
                    } else if query.contains("page=3") {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m2",
                                    "messageType": "message",
                                    "createdDateTime": "2026-11-18T10:03:00Z",
                                    "from": { "user": { "id": "u2", "displayName": "B" } },
                                    "body": { "contentType": "text", "content": "second" }
                                },
                                {
                                    "id": "m1",
                                    "messageType": "message",
                                    "createdDateTime": "2026-11-18T10:02:00Z",
                                    "from": { "user": { "id": "u1", "displayName": "A" } },
                                    "body": { "contentType": "text", "content": "first" }
                                }
                            ],
                            "@odata.nextLink": p4_url
                        }))
                    } else if query.contains("page=4") {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "m0",
                                    "messageType": "message",
                                    "createdDateTime": "2026-11-18T10:01:00Z",
                                    "from": { "user": { "id": "u1", "displayName": "A" } },
                                    "body": { "contentType": "text", "content": "oldest" }
                                }
                            ]
                        }))
                    } else {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "sys-1",
                                    "messageType": "systemEventMessage",
                                    "createdDateTime": "2026-11-18T10:05:00Z",
                                    "body": { "contentType": "html", "content": "<systemEventMessage/>" }
                                }
                            ],
                            "@odata.nextLink": p2_url
                        }))
                    }
                }
            }),
        );

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = crate::config::AppConfig {
            graph_base_url: base,
            ..Default::default()
        };
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

        let (page, next) = client.list_chat_messages_page("c1", 20, None).await.unwrap();
        let page_ids: Vec<&str> = page.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(page_ids, vec!["m1", "m2"]);
        assert_eq!(next.as_deref(), Some(p4_url.as_str()));

        let all = client.list_chat_messages("c1", 10).await.unwrap();
        let all_ids: Vec<&str> = all.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(all_ids, vec!["m0", "m1", "m2"]);
    }

    #[test]
    fn batch_chat_messages_report_failed_subresponses_as_none() {
        let chat_ids = vec!["c-ok".to_string(), "c-throttled".to_string(), "c-missing".to_string(), "c-garbled".to_string()];
        let responses: Vec<BatchSubResponse> = serde_json::from_value(serde_json::json!([
            {
                "id": "1",
                "status": 429,
                "headers": { "Retry-After": "5" },
                "body": { "error": { "code": "TooManyRequests", "message": "throttled" } }
            },
            {
                "id": "0",
                "status": 200,
                "body": {
                    "value": [{
                        "id": "m1",
                        "createdDateTime": "2026-11-20T09:00:00Z",
                        "messageType": "message",
                        "from": { "user": { "id": "u2", "displayName": "Alice" } },
                        "body": { "contentType": "html", "content": "<p>hi</p>" }
                    }]
                }
            },
            { "id": "3", "status": 200, "body": { "value": "not a list" } }
        ]))
        .unwrap();

        let out = collect_batch_chat_messages(&chat_ids, responses, Some("u1"), 20);
        let ids: Vec<&str> = out.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["c-ok", "c-throttled", "c-missing", "c-garbled"]);
        let ok = out[0].1.as_ref().expect("2xx sub-response yields messages");
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].msg_id, "m1");
        assert_eq!(ok[0].conv_key, "chat:c-ok");
        assert!(out[1].1.is_none(), "429 sub-response is a failure, not an empty chat");
        assert!(out[2].1.is_none(), "a missing sub-response is a failure");
        assert!(out[3].1.is_none(), "an unparseable body is a failure");

        // A real empty chat is `Some(vec![])`.
        let empty: Vec<BatchSubResponse> = serde_json::from_value(serde_json::json!([
            { "id": "0", "status": 200, "body": { "value": [] } }
        ]))
        .unwrap();
        let out = collect_batch_chat_messages(&chat_ids[..1], empty, Some("u1"), 20);
        assert_eq!(out[0].1.as_deref(), Some(&[][..]));
    }

    /// Mock `/me/chats` with Graph-style paging: the nextLink carries the first page's `$top`.
    async fn spawn_paged_chats_server(
        total: usize,
        ignore_top: bool,
    ) -> (GraphClient, tokio::task::JoinHandle<()>) {
        use axum::{extract::RawQuery, routing::get, Json, Router};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}/v1.0");
        let route_base = base.clone();
        let app = Router::new().route(
            "/v1.0/me/chats",
            get(move |RawQuery(query): RawQuery| {
                let base = route_base.clone();
                async move {
                    let q = query.unwrap_or_default();
                    let param = |name: &str| {
                        q.split('&').find_map(|kv| {
                            let (k, v) = kv.split_once('=')?;
                            (k == name).then(|| v.parse::<usize>().ok()).flatten()
                        })
                    };
                    let top = if ignore_top { 50 } else { param("$top").unwrap_or(50) };
                    let skip = param("$skiptoken").unwrap_or(0);
                    let page: Vec<serde_json::Value> = (skip..total.min(skip + top))
                        .map(|i| {
                            serde_json::json!({
                                "id": format!("c{i:03}"),
                                "chatType": "group",
                                "topic": format!("Chat {i}"),
                                "lastUpdatedDateTime": "2026-11-21T09:00:00Z",
                                "members": []
                            })
                        })
                        .collect();
                    let next_skip = skip + page.len();
                    let mut body = serde_json::json!({ "value": page });
                    if next_skip < total {
                        body["@odata.nextLink"] = serde_json::Value::String(format!(
                            "{base}/me/chats?$top={top}&$skiptoken={next_skip}"
                        ));
                    }
                    Json(body)
                }
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = crate::config::AppConfig {
            graph_base_url: base,
            ..crate::config::AppConfig::default()
        };
        let tokens = crate::auth::StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: cfg.client_id.clone(),
            tenant_id: cfg.tenant_id.clone(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Me".to_string()),
            user_email: None,
            auth_mode: "pkce".to_string(),
        };
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("u1");
        (graph, server)
    }

    // TEAMS-08: with limit 70 the second page (50 rows) is cut after 20. The cursor must lead
    // back to that page; the old cursor pointed at page 3 and rows 70..100 were never listed.
    #[tokio::test]
    async fn regression_teams_08_chats_cursor_no_skip() {
        let (graph, server) = spawn_paged_chats_server(120, false).await;

        let first = graph.list_chats_with_cursor(70).await.unwrap();
        assert_eq!(first.chats.len(), 70);
        assert!(!first.eof);
        let mut seen: HashSet<String> = first.chats.iter().map(|c| c.conv_key.clone()).collect();
        assert_eq!(seen.len(), 70);

        // Follow the cursor the way the chat list does (`list_chats_page` until no link).
        let mut cursor = first.next_link.clone();
        assert!(cursor.as_deref().is_some_and(|l| l.contains("$skiptoken=50")), "{cursor:?}");
        let mut pages = 0;
        while let Some(link) = cursor {
            let (chats, next) = graph.list_chats_page(50, Some(&link)).await.unwrap();
            seen.extend(chats.into_iter().map(|c| c.conv_key));
            cursor = next;
            pages += 1;
            assert!(pages <= 5, "cursor must reach the end");
        }
        let expected: HashSet<String> = (0..120).map(|i| format!("chat:c{i:03}")).collect();
        assert_eq!(seen, expected, "following the cursor must not skip any chat");

        server.abort();
    }

    #[tokio::test]
    async fn chats_cursor_is_unchanged_when_no_page_is_cut() {
        let (graph, server) = spawn_paged_chats_server(120, false).await;

        // A limit that ends on a page boundary: the cursor is the next page, as before.
        let res = graph.list_chats_with_cursor(100).await.unwrap();
        assert_eq!(res.chats.len(), 100);
        assert!(!res.eof);
        assert!(
            res.next_link.as_deref().is_some_and(|l| l.contains("$skiptoken=100")),
            "{:?}",
            res.next_link
        );

        // Everything fits: end of list, no cursor.
        let all = graph.list_chats_with_cursor(500).await.unwrap();
        assert_eq!(all.chats.len(), 120);
        assert!(all.eof);
        assert!(all.next_link.is_none());

        server.abort();
    }

    #[tokio::test]
    async fn chats_cursor_restarts_when_first_page_over_returns() {
        // A server that ignores `$top` cuts the very first page: there is no link to re-read,
        // so the result says "not at the end, no cursor" and the caller starts over.
        let (graph, server) = spawn_paged_chats_server(120, true).await;
        let res = graph.list_chats_with_cursor(30).await.unwrap();
        assert_eq!(res.chats.len(), 30);
        assert!(res.next_link.is_none());
        assert!(!res.eof, "a cut first page is not the end of the list");
        server.abort();
    }

    #[tokio::test]
    async fn search_users_memoizes_filter_unsupported_and_skips_fallback_on_403() {
        use axum::{extract::Query, http::StatusCode, routing::get, Json, Router};
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        // 1) Server rejects `$filter` with 400 Request_UnsupportedQuery and supports plain `$top`.
        let filter_hits = Arc::new(AtomicUsize::new(0));
        let fallback_hits = Arc::new(AtomicUsize::new(0));
        let fh = Arc::clone(&filter_hits);
        let bh = Arc::clone(&fallback_hits);
        let router = Router::new().route(
            "/v1.0/users",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let fh = Arc::clone(&fh);
                let bh = Arc::clone(&bh);
                async move {
                    if q.contains_key("$filter") {
                        fh.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": {
                                    "code": "Request_UnsupportedQuery",
                                    "message": "Unsupported query"
                                }
                            })),
                        )
                    } else {
                        bh.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    { "id": "u1", "displayName": "Alice Smith", "mail": "alice@example.com" },
                                    { "id": "u2", "displayName": "Bob Jones", "mail": "bob@example.com" }
                                ]
                            })),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let cfg = crate::config::AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..Default::default()
        };
        let tokens = crate::auth::StoredTokens {
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
        let auth = crate::auth::AuthManager::from_tokens(cfg.clone(), tokens.clone());
        let client = GraphClient::new(cfg, auth).unwrap().with_my_user_id("me");

        let r1 = client.search_users("ali", 10).await.unwrap();
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0].display_name, "Alice Smith");
        let r2 = client.search_users("bob", 10).await.unwrap();
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].display_name, "Bob Jones");

        assert_eq!(
            filter_hits.load(Ordering::SeqCst),
            1,
            "$filter failure should be memoized after the first unsupported error"
        );
        assert_eq!(fallback_hits.load(Ordering::SeqCst), 2);
        server.abort();

        // 2) Server returns 403 Forbidden: must NOT issue a wasted second fallback call.
        let forbidden_hits = Arc::new(AtomicUsize::new(0));
        let f403 = Arc::clone(&forbidden_hits);
        let router_403 = Router::new().route(
            "/v1.0/users",
            get(move || {
                let f403 = Arc::clone(&f403);
                async move {
                    f403.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({
                            "error": { "code": "Authorization_RequestDenied", "message": "Missing User.ReadBasic.All" }
                        })),
                    )
                }
            }),
        );
        let listener2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = listener2.local_addr().unwrap();
        let server2 = tokio::spawn(async move {
            let _ = axum::serve(listener2, router_403).await;
        });
        let cfg2 = crate::config::AppConfig {
            graph_base_url: format!("http://{addr2}/v1.0"),
            ..Default::default()
        };
        let auth2 = crate::auth::AuthManager::from_tokens(cfg2.clone(), tokens);
        let client2 = GraphClient::new(cfg2, auth2).unwrap().with_my_user_id("me");

        assert!(client2.search_users("ali", 10).await.is_err());
        assert_eq!(
            forbidden_hits.load(Ordering::SeqCst),
            1,
            "403 Forbidden must return immediately without a fallback request"
        );
        server2.abort();
    }
}
