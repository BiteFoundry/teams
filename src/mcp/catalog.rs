//! MCP tool catalog definitions (16 Microsoft Teams tools).

use serde_json::{json, Value};

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

pub(super) fn tool_catalog() -> Vec<Value> {
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
