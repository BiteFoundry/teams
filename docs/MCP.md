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
- **`serverInfo`**: `{ "name": "teams", "version": "0.1.0" }` (matches `CARGO_PKG_VERSION`).
- **Startup**: `initialize` responds immediately; on an empty cache, initial conversation bootstrap runs asynchronously in the background.
- **Notifications**: requests without `id` or methods starting with `notifications/` (such as `notifications/initialized`) emit no response.
- **`ping`**: returns `{}`.
- **Error codes**: `-32700` parse error, `-32600` invalid JSON-RPC envelope (or oversized line > `8 MiB`), `-32601` method not found.
- **`tools/call`**: returns `{ "content": [{ "type": "text", "text": "<pretty JSON>" }], "isError": false }` on success, or `"isError": true` with an error message (including JWT `scp`/`roles` permission diagnostics on `403`) on validation/Graph failures.

## Microsoft Teams Tools (17)

`target` arguments accept a canonical `conv_key` (`chat:...` or `chan:<team_id>:<channel_id>`), raw `chat_id`, `"Team/Channel"`, or a unique case-insensitive conversation title substring.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_chats` | `refresh?` (bool), `limit?` (1..1000) | `{ "chats": [...] }` |
| `create_chat` | `members` (`string[]`, min 1), `topic?` | `{ "ok": true, "conversation": {...} }` |
| `list_teams_channels` | `refresh?` (bool) | `{ "teams": [...] }` |
| `get_messages` | `target`, `limit?` (1..1000), `before?` (ISO-8601 timestamp, `YYYY-MM-DD`, or `msg_id`), `offline?` (bool) | `{ "conversation": {...}, "messages": [...], "has_more": bool, "next_before": string \| null }` |
| `get_thread` | `target`, `root_msg_id`, `limit?` (1..1000) | `{ "conversation": {...}, "thread": {...} }` |
| `send_message` | `target`, `markdown`, `subject?`, `mentions?`, `important?` | `{ "ok": true, "conversation": {...}, "message": {...} }` |
| `reply_message` | `target`, `msg_id`, `markdown`, `mentions?`, `important?` | `{ "ok": true, "conversation": {...}, "reply_to": "...", "message": {...} }` |
| `send_file` | `target`, `path`, `caption?`, `thread_root_id?`, `inline?` | `{ "ok": true, "conversation": {...}, "path": "...", "message": {...} }` |
| `download_attachment` | `target`, `msg_id`, `index?`, `out_dir?`, `thread_root_id?` | `{ "ok": true, "path": "...", "file_name": "...", "bytes_written": ... }` |
| `react_message` | `target`, `msg_id`, `reaction`, `enable?` (default `true`), `thread_root_id?` | `{ "ok": true, "conv_key": "...", "msg_id": "...", "reaction": "...", "enable": ... }` |
| `edit_message` | `target`, `msg_id`, `markdown`, `thread_root_id?` | `{ "ok": true, "conv_key": "...", "msg_id": "..." }` |
| `delete_message` | `target`, `msg_id`, `confirm` (must be literal `true`), `thread_root_id?` | `{ "ok": true, "conv_key": "...", "msg_id": "..." }` |
| `search_messages` | `query`, `target?`, `limit?` (1..1000) | `{ "query": "...", "hits": [...] }` |
| `list_members` | `target` | `{ "conv_key": "...", "title": "...", "members": [...] }` |
| `mark_read` | `target` | `{ "ok": true, "conv_key": "..." }` |
| `get_presence` | `user_ids?` (`string[]`) | `{ "presence": {...} }` or `{ "presences": [...] }` |
| `set_presence` | `availability` (`Available\|Busy\|DoNotDisturb\|BeRightBack\|Away\|Reset`), `activity?`, `expiration_mins?` (1..1440) | `{ "ok": true, "availability": "...", "activity": "...", "expiration_mins": ... }` |

### Mentions and importance in `send_message` and `reply_message`

`mentions` accepts either `"user_id:DisplayName"` strings or `{ "id"?: 0, "user_id": "...", "display_name": "..." }` objects. Set `important: true` to mark the message with high importance.

### Destructive tool guard (`confirm: true`)

`delete_message` requires `"confirm": true` as a literal JSON boolean. Omitting `confirm`, passing `false`, `"true"`, or `1` is rejected during argument validation before any Graph API call is made.

### Example JSON-RPC session

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"agent","version":"1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list"}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_chats","arguments":{"refresh":true,"limit":100}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get_messages","arguments":{"target":"Engineering/General","before":"2026-11-01","limit":50}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"send_message","arguments":{"target":"Engineering/General","markdown":"Build **passed**!","subject":"CI","mentions":["u1:Alice"]}}}
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"delete_message","arguments":{"target":"Engineering/General","msg_id":"1700000000000","confirm":true}}}
```

## Notes and limitations

- `send_file` reads `path` on the host running `teams mcp`, not on a remote MCP client.
- `get_messages` with `before` automatically walks Microsoft Graph `@odata.nextLink` (`$skiptoken`) pages when `offline` is `false` (default) and caches fetched messages into `cache.db`.
- `search_messages` queries the local SQLite FTS5 cache (`cache.db`); run `teams sync --depth 100` or `get_messages` first if local history is empty.
- `react_message` supports `like`, `heart`, `laugh`, `surprised`, `sad`, `angry`, and unicode emoji.
