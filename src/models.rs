//! Shared domain models and wire-independent contracts across Graph, Storage, Parser, Sync, TUI, CLI, HTTP, and MCP.
//!
//! IMPORTANT: Do NOT break existing field names or signatures in this file when working in a module,
//! as all sibling modules compile against these exact types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Kind of Teams conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConversationKind {
    OneOnOne,
    Group,
    Meeting,
    Channel,
}

impl ConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneOnOne => "oneOnOne",
            Self::Group => "group",
            Self::Meeting => "meeting",
            Self::Channel => "channel",
        }
    }

    pub fn from_db_str(value: &str) -> Self {
        match value {
            "oneOnOne" | "one_on_one" => Self::OneOnOne,
            "group" => Self::Group,
            "meeting" => Self::Meeting,
            "channel" => Self::Channel,
            _ => Self::Group,
        }
    }

    pub fn is_channel(self) -> bool {
        matches!(self, Self::Channel)
    }
}

/// Strongly typed address for either a 1:1/group/meeting Chat or a Team Channel.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "target_type", rename_all = "snake_case")]
pub enum ConversationTarget {
    Chat {
        chat_id: String,
    },
    Channel {
        team_id: String,
        channel_id: String,
    },
}

impl ConversationTarget {
    /// Canonical primary key stored in SQLite and used in URLs (`chat:<chat_id>` or `chan:<team_id>:<channel_id>`).
    pub fn conv_key(&self) -> String {
        match self {
            Self::Chat { chat_id } => format!("chat:{chat_id}"),
            Self::Channel {
                team_id,
                channel_id,
            } => format!("chan:{team_id}:{channel_id}"),
        }
    }

    pub fn from_conv_key(key: &str) -> Option<Self> {
        if let Some(chat_id) = key.strip_prefix("chat:") {
            if !chat_id.is_empty() {
                return Some(Self::Chat {
                    chat_id: chat_id.to_string(),
                });
            }
        }
        if let Some(rest) = key.strip_prefix("chan:") {
            let (team_id, channel_id) = rest.split_once(':')?;
            if !team_id.is_empty() && !channel_id.is_empty() {
                return Some(Self::Channel {
                    team_id: team_id.to_string(),
                    channel_id: channel_id.to_string(),
                });
            }
        }
        None
    }

    pub fn is_channel(&self) -> bool {
        matches!(self, Self::Channel { .. })
    }
}

/// Summary row for a chat or channel in navigation lists, CLI `chats`/`channels`, HTTP, and MCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub conv_key: String,
    pub target: ConversationTarget,
    pub kind: ConversationKind,
    pub team_id: Option<String>,
    pub team_name: Option<String>,
    pub title: String,
    pub topic: Option<String>,
    pub unread_count: i32,
    pub last_read_at: Option<String>,
    pub last_msg_id: Option<String>,
    pub last_msg_author: Option<String>,
    pub last_msg_preview: Option<String>,
    pub last_updated_at: Option<String>,
    pub pinned: bool,
    /// For 1:1 chats, the other participant's Entra user ID (used for live presence dots).
    pub peer_user_id: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelSummary {
    pub id: String,
    pub team_id: String,
    pub team_name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub membership_type: String,
    pub web_url: Option<String>,
}

impl ChannelSummary {
    pub fn target(&self) -> ConversationTarget {
        ConversationTarget::Channel {
            team_id: self.team_id.clone(),
            channel_id: self.id.clone(),
        }
    }

    pub fn conv_key(&self) -> String {
        self.target().conv_key()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamSummary {
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub channels: Vec<ChannelSummary>,
}

/// File, reference, inline hosted image, or Adaptive Card attached to a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub id: String,
    pub content_type: String,
    pub content_url: Option<String>,
    pub name: Option<String>,
    /// JSON payload when `content_type` is `application/vnd.microsoft.card.adaptive` or similar.
    pub content_json: Option<String>,
    pub thumbnail_url: Option<String>,
}

/// `@mention` attached to a Teams message (`<at id="{id}">{display_name}</at>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MentionInfo {
    pub id: i32,
    pub user_id: String,
    pub display_name: String,
}

/// Aggregated reaction badge on a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionSummary {
    /// Graph reactionType (`like`, `angry`, `sad`, `laugh`, `heart`, `surprised`, or unicode emoji).
    pub reaction_type: String,
    pub count: usize,
    pub me_reacted: bool,
    pub users: Vec<String>,
}

/// Normalized message item across 1:1 chats, group chats, channel root posts, and channel thread replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageItem {
    /// Primary key `"{conv_key}|{msg_id}"`.
    pub msg_key: String,
    pub conv_key: String,
    pub msg_id: String,
    /// `Some(root_msg_id)` when this message is a reply inside a Team Channel thread; `None` for root posts & chats.
    pub parent_msg_id: Option<String>,
    /// Quoted message ID for 1:1 / group chat replies (`messageReference` / `blockquote`).
    pub reply_to_id: Option<String>,
    pub reply_to_author: Option<String>,
    pub reply_to_preview: Option<String>,
    /// Optional subject line on Team Channel root posts.
    pub subject: Option<String>,
    pub author_id: Option<String>,
    pub author_name: String,
    pub is_me: bool,
    pub created_at: String,
    pub last_modified_at: Option<String>,
    pub deleted: bool,
    pub importance: String,
    pub message_type: String,
    pub body_html: String,
    pub body_plain: String,
    pub attachments: Vec<AttachmentInfo>,
    pub mentions: Vec<MentionInfo>,
    pub reactions: Vec<ReactionSummary>,
    /// Number of thread replies under a channel root post.
    pub reply_count: usize,
    pub web_url: Option<String>,
}

impl MessageItem {
    pub fn make_msg_key(conv_key: &str, msg_id: &str) -> String {
        format!("{conv_key}|{msg_id}")
    }

    /// Returns `self.attachments` augmented on-the-fly with any inline `<img src="...">` images
    /// from `self.body_html` not already represented in `self.attachments`.
    /// Fast path: returns `Cow::Borrowed(&self.attachments)` in O(1) when `self.body_html` contains no `<img`/`<IMG`.
    pub fn effective_attachments(&self) -> std::borrow::Cow<'_, [AttachmentInfo]> {
        if !self
            .body_html
            .as_bytes()
            .windows(4)
            .any(|w| w.eq_ignore_ascii_case(b"<img"))
        {
            return std::borrow::Cow::Borrowed(&self.attachments);
        }
        let extra = crate::parser::extract_inline_image_attachments(
            &self.body_html,
            &self.attachments,
            &self.conv_key,
            &self.msg_id,
            self.parent_msg_id.as_deref(),
        );
        if extra.is_empty() {
            std::borrow::Cow::Borrowed(&self.attachments)
        } else {
            let mut combined = Vec::with_capacity(self.attachments.len() + extra.len());
            combined.extend(self.attachments.iter().cloned());
            combined.extend(extra);
            std::borrow::Cow::Owned(combined)
        }
    }
}

/// Thread bundle: root channel post + ordered replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadView {
    pub conv_key: String,
    pub root: MessageItem,
    pub replies: Vec<MessageItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceStatus {
    pub user_id: String,
    /// `Available`, `Busy`, `DoNotDisturb`, `Away`, `BeRightBack`, `Offline`, `PresenceUnknown`.
    pub availability: String,
    pub activity: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub display_name: String,
    pub mail: Option<String>,
    pub user_principal_name: Option<String>,
}

impl AttachmentInfo {
    pub fn is_message_reference(&self) -> bool {
        self.content_type == "messageReference"
    }
}

/// What a thread read knows about replies beyond the returned window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadCoverage {
    /// Replies exist that this read did not return.
    pub has_more: bool,
    /// Total replies in the thread; None when the source did not establish it.
    pub reply_total: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TeamChannelError {
    pub team_id: String,
    pub display_name: String,
    /// Status of the failed $batch sub-response; None when the response was missing.
    pub status: Option<u16>,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedTeams {
    pub teams: Vec<TeamSummary>,
    pub channel_errors: Vec<TeamChannelError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub user_id: String,
    pub member_id: Option<String>,
    pub display_name: String,
    pub email: Option<String>,
    pub roles: Vec<String>,
    pub presence: Option<PresenceStatus>,
}

/// Outgoing reference attachment after uploading to OneDrive / SharePoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadedReferenceAttachment {
    /// GUID extracted from DriveItem `eTag` (or generated UUID fallback if absent).
    pub id: String,
    pub name: String,
    /// `webDavUrl` or sharing `webUrl` for Graph `reference` attachment.
    pub content_url: String,
    pub size: u64,
    pub drive_item_id: Option<String>,
}

/// Outgoing inline image (`chatMessageHostedContent`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineImageUpload {
    /// Temporary integer/string identifier referenced as `../hostedContents/{temp_id}/$value`.
    pub temp_id: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub file_name: Option<String>,
}

/// Compiled outgoing payload ready to POST/PATCH to Microsoft Graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub subject: Option<String>,
    pub body_html: String,
    pub mentions: Vec<MentionInfo>,
    pub attachments: Vec<UploadedReferenceAttachment>,
    pub inline_images: Vec<InlineImageUpload>,
    pub importance: Option<String>,
    /// Optional quoted message for 1:1/group chat replies.
    pub quote_reply_to: Option<QuotedReply>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotedReply {
    pub msg_id: String,
    pub author_name: String,
    pub preview_plain: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedFile {
    pub path: PathBuf,
    pub file_name: String,
    pub bytes_written: u64,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub is_image: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    pub conv_key: String,
    pub conv_title: String,
    pub kind: ConversationKind,
    pub msg_id: String,
    pub parent_msg_id: Option<String>,
    pub author_name: String,
    pub created_at: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCursor {
    pub scope_key: String,
    pub delta_link: Option<String>,
    pub oldest_msg_ts: Option<String>,
    pub synced_at: String,
}

impl SyncCursor {
    pub const HISTORY_EOF: &'static str = "EOF";
    pub const CHATS_LIST_SCOPE: &'static str = "chats_list";

    pub fn history_scope(conv_key: &str) -> String {
        format!("history:{conv_key}")
    }

    pub fn is_history_eof(&self) -> bool {
        self.delta_link.is_none() && self.oldest_msg_ts.as_deref() == Some(Self::HISTORY_EOF)
    }
}

/// Sentinel summary emitted via `CoreEvent::SyncStateChanged { busy: false, summary }`
/// while background polling is silently retrying transient errors.
pub const POLL_RETRY_SUMMARY: &str = "__poll_retrying__";

/// Asynchronous events pushed from `SyncEngine` to TUI or live subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CoreEvent {
    ConversationsUpdated(Vec<ConversationSummary>),
    TeamsUpdated(Vec<TeamSummary>),
    MessagesUpdated {
        conv_key: String,
        parent_msg_id: Option<String>,
        messages: Vec<MessageItem>,
        notify_candidate: Option<NotificationCandidate>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changed_msg_ids: Option<Vec<String>>,
    },
    PresenceUpdated(Vec<PresenceStatus>),
    SyncStateChanged {
        busy: bool,
        summary: String,
    },
    SyncError(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationCandidate {
    pub conv_key: String,
    pub conv_title: String,
    pub sender: String,
    pub preview: String,
}

// ---------------------------------------------------------------------------
// Parser Rich AST (shared between `parser` and `ui`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStyleFlags {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub mention: bool,
    pub link_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichSpan {
    pub text: String,
    pub style: TextStyleFlags,
}

impl RichSpan {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: TextStyleFlags::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RichBlock {
    Paragraph(Vec<RichSpan>),
    Heading {
        level: u8,
        spans: Vec<RichSpan>,
    },
    Quote(Vec<RichSpan>),
    CodeBlock {
        lang: Option<String>,
        code: String,
    },
    BulletList(Vec<Vec<RichSpan>>),
    Card {
        title: Option<String>,
        subtitle: Option<String>,
        facts: Vec<(String, String)>,
        body_lines: Vec<String>,
        actions: Vec<(String, String)>,
    },
    AttachmentChip {
        id: String,
        name: Option<String>,
    },
    InlineImageChip {
        src: String,
        index: usize,
    },
}

// ---------------------------------------------------------------------------
// Self-Updater Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub updated: bool,
    pub binary_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn sample_message(conv_key: &str, msg_id: &str, body_html: &str) -> MessageItem {
        MessageItem {
            msg_key: MessageItem::make_msg_key(conv_key, msg_id),
            conv_key: conv_key.to_string(),
            msg_id: msg_id.to_string(),
            parent_msg_id: None,
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: None,
            author_id: None,
            author_name: "Alice".to_string(),
            is_me: false,
            created_at: "2026-11-18T00:00:00Z".to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: body_html.to_string(),
            body_plain: String::new(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        }
    }

    #[test]
    fn effective_attachments_fast_path_and_inline_synthesis() {
        let plain_msg = sample_message("chat:19:abc", "m1", "<p>hello</p>");
        assert!(matches!(plain_msg.effective_attachments(), Cow::Borrowed(_)));
        assert!(plain_msg.effective_attachments().is_empty());

        let hosted_msg = sample_message(
            "chat:19:abc",
            "m1",
            r#"<p><img src="https://graph.microsoft.com/v1.0/chats/19:abc/messages/m1/hostedContents/aWQ9MQ==/$value"></p>"#,
        );
        let eff = hosted_msg.effective_attachments();
        assert!(matches!(eff, Cow::Owned(_)));
        assert_eq!(eff.len(), 1);
        assert_eq!(eff[0].id, "hosted:aWQ9MQ==");
        assert_eq!(eff[0].content_type, "hostedContent");
        assert!(crate::img_preview::is_image_mime_or_name(
            &eff[0].content_type,
            eff[0].name.as_deref()
        ));

        // When already persisted in `attachments`, effective_attachments borrows without duplicating.
        let owned = eff.into_owned();
        let mut persisted = hosted_msg;
        persisted.attachments = owned;
        assert!(matches!(persisted.effective_attachments(), Cow::Borrowed(_)));
        assert_eq!(persisted.effective_attachments().len(), 1);

        // Relative ../hostedContents/1/$value resolution for chats
        let rel_msg = sample_message(
            "chat:19:abc",
            "m1",
            r#"<p><img src="../hostedContents/1/$value"></p>"#,
        );
        let rel_eff = rel_msg.effective_attachments();
        assert_eq!(rel_eff.len(), 1);
        assert_eq!(
            rel_eff[0].content_url.as_deref(),
            Some("/chats/19%3Aabc/messages/m1/hostedContents/1/$value")
        );
    }
}
