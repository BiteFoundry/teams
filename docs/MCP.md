# `teams` MCP Server

`teams mcp` (alias `mcp-server`) runs a [Model Context Protocol](https://modelcontextprotocol.io/) server over **stdio** using newline-delimited JSON-RPC 2.0. It exposes pure **Microsoft Teams** capabilities (1:1/group/meeting chats, teams & channels, threads, deep `@odata.nextLink` message history, OneDrive/SharePoint file & inline image uploads, attachment downloads, reactions, FTS5 search, and Presence) to MCP-capable agents such as Claude Desktop, Claude Code, and Codex.

## Requirements

1. Build or install the `teams` binary:
   ```bash
   npm install -g @bitefoundry/teams
   # or from source:
   cargo build --release
   ```
2. Authenticate once (the MCP server never prompts interactively on stdin):
   ```bash
   teams login
   ```
   Or configure `TEAMS_ACCESS_TOKEN` / `TEAMS_TOKEN_FILE` (+ optional `TEAMS_TOKEN_REFRESH_CMD`) in the environment or `config.toml`.
3. Ensure `teams` (e.g. `target/release/teams.exe` on Windows or on `PATH`) is accessible to your MCP host.

## Starting manually

```bash
teams mcp          # stdio: one JSON-RPC 2.0 object per line
teams mcp-server   # alias
```

`stdout` is strictly reserved for single-line JSON-RPC 2.0 responses; diagnostics and startup warnings are written to `stderr` only.

## Client configuration

### Claude Desktop

`claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "teams": {
      "command": "teams",
      "args": ["mcp"]
    }
  }
}
```

On Windows with an explicit executable path:

```json
{
  "mcpServers": {
    "teams": {
      "command": "C:\\Users\\me\\source\\teams\\target\\release\\teams.exe",
      "args": ["mcp"]
    }
  }
}
```

### Claude Code

```bash
claude mcp add teams -- teams mcp
# or with an explicit binary path:
claude mcp add teams -- C:\Users\me\source\teams\target\release\teams.exe mcp
```

### Codex

`~/.codex/config.toml`:

```toml
[mcp_servers.teams]
command = "teams"
args = ["mcp"]
```

## Protocol

- **Transport**: JSON-RPC 2.0 over stdio, **newline-delimited JSON** (one JSON object per line, no `Content-Length` headers; maximum `8 MiB` per request line).
- **Supported `protocolVersion`s**: `2024-11-05`, `2025-03-26`, `2025-06-18` (unknown versions fall back to `2025-06-18`).
- **`serverInfo`** reports the name `teams` and the running binary's version.
- **Startup** opens the local cache. Online tools fetch from Graph when their requests need it.
- **Notifications**: requests without `id` or methods starting with `notifications/` (such as `notifications/initialized`) emit no response.
- **`ping`**: returns `{}`.
- **Error codes**: `-32700` parse error, `-32600` invalid JSON-RPC envelope (or oversized line > `8 MiB`), `-32601` method not found.
- **`tools/call`**: returns `{ "content": [{ "type": "text", "text": "<pretty JSON>" }], "isError": false }` on success, or `"isError": true` with `content[0].text` set to a compact JSON error object `{"ok":false,"error":{"code":"...","message":"...","http_status"?:N}}` on validation/Graph failures (including JWT `scp`/`roles` permission diagnostics on read `403`, or write-scope guidance on write `403`).

`error.code` uses the same classification as CLI `--json` runtime errors:

| `code` | Meaning | Extra fields |
| --- | --- | --- |
| `target_ambiguous` | Target matched multiple chats/channels (CLI `--json` includes `candidates`) | `candidates` (CLI) |
| `target_not_found` | Target conversation/channel was not found | |
| `not_signed_in` | No active sign-in token | |
| `token_expired` | Token expired | `details` (CLI `whoami`) |
| `permission_denied` | Graph `403` / insufficient permissions | `http_status: 403` |
| `unauthorized` | Graph `401` | `http_status: 401` |
| `not_found` | Graph `404` | `http_status: 404` |
| `error` | Validation, missing confirmation, or other errors | `http_status` (when from Graph) |

## Microsoft Teams Tools (16)

`target` arguments accept a canonical `conv_key` (`chat:...` or `chan:<team_id>:<channel_id>`), raw `chat_id`, `"Team/Channel"`, or a unique case-insensitive conversation title substring.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_chats` | `refresh?` (bool), `limit?` (1..1000) | `{ "chats": [...] }` |
| `create_chat` | `members` (`string[]`, min 1), `topic?` | `{ "ok": true, "conversation": {...} }` |
| `update_chat` | Group chat `target`, `topic` (1–250 characters) | `{ "ok": true, "conversation": {...} }` |
| `list_teams_channels` | `refresh?` (bool), `limit?` (1..1000, default `50`), `cursor?` (conflicts with `refresh: true`) | `{ "teams": [...], "returned": int, "has_more": bool, "next_cursor": string \| null, "total": int, "channel_errors"?: [...] }` |
| `get_messages` | `target`, `limit?` (1..1000), `before?` (ISO-8601 timestamp, `YYYY-MM-DD`, or `msg_id`), `offline?`, `compact?` (default `true`) | `{ "conversation": {...}, "messages": [...], "has_more": bool, "next_before": string \| null, "omitted"?: {...}, "hint"?: string }` |
| `get_thread` | `target`, `root_msg_id`, `limit?` (1..1000 replies), `offline?`, `compact?` (default `true`) | `{ "conversation": {...}, "thread": { "root": {...}, "replies": [...], "returned": int, "has_more": bool }, "omitted"?: {...}, "hint"?: string }` |
| `send_message` | `target`, `markdown`, `subject?`, `mentions?`, `important?`, `compact?` (default `true`) | `{ "ok": true, "conversation": {...}, "message": {...} }` (compact ack by default; set `compact: false` for full objects) |
| `reply_message` | `target`, `msg_id`, `markdown`, `mentions?`, `important?`, `compact?` (default `true`) | `{ "ok": true, "conversation": {...}, "reply_to": "...", "message": {...} }` (compact ack by default; set `compact: false` for full objects) |
| `send_file` | `target`, `path`, `caption?`, `thread_root_id?`, `inline?`, `compact?` (default `true`) | `{ "ok": true, "conversation": {...}, "path": "...", "inline": bool, "message": {...} }` (compact ack by default; set `compact: false` for full objects) |
| `download_attachment` | `target`, `msg_id`, `index?`, `out_dir?`, `thread_root_id?` | `{ "ok": true, "path": "...", "file_name": "...", "bytes_written": ... }` |
| `edit_message` | `target`, `msg_id`, and one mode. Body modes use `markdown` or both `find` and `replace`, with optional `thread_root_id`. The title mode uses `subject`. | `{ "ok": true, "conv_key": "...", "msg_id": "..." }` |
| `delete_message` | `target`, `msg_id`, `confirm` (must be literal `true`), `thread_root_id?` | `{ "ok": true, "conv_key": "...", "msg_id": "..." }` |
| `search_messages` | `query`, `target?`, `limit?` (1..1000) | `{ "query": "...", "hits": [...] }` |
| `list_members` | `target`, `limit?` (1..1000, default `50`), `cursor?`, `offline?` (default `false`) | `{ "conv_key": "...", "title": "...", "members": [...], "returned": int, "has_more": bool \| null, "next_cursor": string \| null, "total": int \| null, "fetched_at": string \| null }` |
| `search_users` | `query` (name or email prefix), `limit?` (1..1000, default `10`) | `{ "users": [{ "user_id", "display_name", "email", "user_principal_name" }], "returned": int }` |
| `get_presence` | `user_ids?` (`string[]`) | `{ "presence": {...} }` or `{ "presences": [...] }` |

### Mentions and importance in `send_message` and `reply_message`

`mentions` accepts either `"user_id:DisplayName"` strings or `{ "id"?: 0, "user_id": "...", "display_name": "..." }` objects. Each mentioned user's `@DisplayName` must appear in `markdown` or the call fails before sending. Duplicate display names are assigned in `mentions` order. Set `important: true` to mark the message with high importance.

### Request cost

Tool descriptions state what a call costs, so an agent can prefer cached reads. The counts below are Microsoft Graph requests per call.

| Tool | Cost |
| --- | --- |
| `list_chats` | No request when the cache covers `limit`; `refresh: true` or a short cache costs about 1 request per 50 chats. |
| `list_teams_channels` | No request for a cached read or a `cursor` page; `refresh: true` or an empty cache lists teams (1+ requests), then channels (1 `$batch` per 20 teams). |
| `get_messages` | Online by default: about 1 request per 50 messages; `offline: true` makes none. |
| `get_thread` | Online: 1+ requests (50 replies each); `offline: true` makes none. |
| `list_members` | Fetches the member list first (1+ requests) unless `cursor` or `offline` is set. |
| `send_file` | Time grows with file size: one upload request up to 4 MiB, 3.2 MiB chunks above, then the message post. |
| `download_attachment` | Time grows with attachment size. |
| `search_messages` | No request: local FTS5 over cached messages only. |
| `search_users` | 1 request (2 when the tenant rejects `$filter`). |
| `get_presence` | 1 request (per 650 user IDs). |

### Compact and offline reads

`get_messages` and `get_thread` default to `compact: true`. Set `compact: false` for full conversation and message objects. Compact output returns `conversation: { conv_key, title }`, fixed message fields (`msg_id`, `author_id`, `author_name`, `created_at`, `body_plain`), reply/edit links when present (`last_modified_at`, `parent_msg_id`, `reply_to_id`), non-`messageReference` attachments as `{ id, name }`, and the following conditional keys when non-default:

| Key | Included when | Meaning when absent |
| --- | --- | --- |
| `subject` | Non-blank string | No subject |
| `deleted` | `true` | Not deleted |
| `message_type` | Not `"message"` | `"message"` |
| `importance` | Not `"normal"` (case-insensitive) | `"normal"` |
| `reply_count` | `> 0` (always present on `get_thread` root as number or `null`) | `0` |
| `links` | `body_html` has `<a href>` links whose URL is not verbatim in `body_plain` (`[{ "text", "url" }]`) | No hidden link URLs |

Compact responses include top-level `omitted` (`{ "mentions": N, "reactions": N, "fields": [...] }`) and `hint` (telling how to request `"compact": false`). Individual messages include `"omitted": { "mentions": N, "reactions": N }` only when `mentions + reactions > 0`. Full responses omit `omitted` and `hint`.

The root is included separately from the `get_thread` reply limit in both modes. Replies are returned oldest first. `thread.returned` reports the number of replies in the window, `thread.has_more` indicates whether older replies exist beyond the window, and `thread.root.reply_count` is the total reply count when known or `null` when truncated/unknown. With `offline: true`, the last successful online thread fetch must cover the requested reply limit or have reached the end of the replies. An empty `replies` list indicates the cached thread fetch found zero replies. A root cached through the message list has unknown reply coverage until a thread fetch runs.

For `list_members` and `list_teams_channels`, passing `cursor` reads the next page from the local SQLite cache without calling Microsoft Graph. In `list_members`, a completed online fetch that returned zero members stays valid offline (`members: []`, `total: 0`, `fetched_at` populated), while a conversation whose members were never fetched returns an error when queried with `offline: true`.

### Edit a message you sent

Use `markdown` to replace the entire body. To update one phrase and preserve existing HTML, send `find` and `replace`. The phrase must occur exactly once within a single editable text node in the current Graph body. The replacement is literal text, and an empty string removes the phrase. An ambiguous match, missing phrase, cross-tag match, or unsupported argument returns an error before PATCH. Use `thread_root_id` with a body mode to edit a channel reply.

Use `subject` by itself to update the title of a channel root post you sent. The PATCH contains only the new plain-text subject. The tool reads the saved post again and updates the cache.

Channel edits require delegated `ChannelMessage.ReadWrite` or legacy `Group.ReadWrite.All`. `ChannelMessage.ReadWrite` requires administrator consent. Chat edits require delegated `Chat.ReadWrite`. The author must be signed in and Teams messaging policy must allow edits. The default teams sign-in scopes cover channel reads and sends. Graph permission or policy denials are returned as tool errors. [Microsoft Graph message update](https://learn.microsoft.com/en-us/graph/api/chatmessage-update?view=graph-rest-1.0) · [Graph permissions reference](https://learn.microsoft.com/en-us/graph/permissions-reference#channelmessagereadwrite)

### Change a group chat topic

`update_chat` changes the topic of an existing group chat accessible to the signed-in user. It requires delegated `Chat.ReadWrite`. A topic can contain 1 to 250 characters and cannot contain a colon. Graph confirms the saved topic in the update response. [Microsoft Graph chat update](https://learn.microsoft.com/en-us/graph/api/chat-patch?view=graph-rest-1.0)

### Destructive tool guard (`confirm: true`)

`delete_message` requires `"confirm": true` as a literal JSON boolean. Omitting `confirm`, passing `false`, `"true"`, or `1` is rejected during argument validation before any Graph API call is made.

### Example JSON-RPC session

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agent","version":"1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_chats","arguments":{"refresh":true,"limit":100}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_messages","arguments":{"target":"Engineering/General","before":"2026-11-01","limit":50}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"send_message","arguments":{"target":"Engineering/General","markdown":"@Alice Build **passed**!","subject":"CI","mentions":["u1:Alice"]}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"delete_message","arguments":{"target":"Engineering/General","msg_id":"1700000000000","confirm":true}}}
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"get_thread","arguments":{"target":"Engineering/General","root_msg_id":"1700000000001","limit":2,"offline":true}}}
{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"edit_message","arguments":{"target":"Engineering/General","msg_id":"1700000000001","find":"STATE: STAGED","replace":"STATE: READY"}}}
{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"update_chat","arguments":{"target":"Release handoff","topic":"Canary war room"}}}
{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"edit_message","arguments":{"target":"Engineering/General","msg_id":"1700000000001","subject":"Canary rollout"}}}
```

## Notes and limitations

- `send_file` reads `path` on the host running `teams mcp`, not on a remote MCP client.
- `get_messages` with `before` automatically walks Microsoft Graph `@odata.nextLink` (`$skiptoken`) pages when `offline` is `false` (default) and caches fetched messages into `cache.db`.
- `search_messages` queries the local SQLite FTS5 cache (`cache.db`); run `teams sync --depth 100` or `get_messages` first if local history is empty.
- Reactions, read receipts (`mark_read`), and presence changes (`set_presence`) remain available in the CLI, TUI, and local HTTP API, and are intentionally omitted from MCP to keep agent tool surfaces focused and prevent unintended unread-state or status changes.
