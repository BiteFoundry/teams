//! MCP tool argument validation and typed argument extraction.

use serde_json::Value;

use crate::{cli, models::MentionInfo};

use super::{catalog::tool_catalog, format_error};

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

fn validate_schema_additional_properties(name: &str, arguments: &Value) -> Result<(), String> {
    use std::collections::{HashMap, HashSet};
    use std::sync::OnceLock;

    static SCHEMA_PROPERTIES: OnceLock<HashMap<String, (HashSet<String>, bool)>> = OnceLock::new();
    let map = SCHEMA_PROPERTIES.get_or_init(|| {
        let mut out = HashMap::new();
        for tool in tool_catalog() {
            let Some(tool_name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            let schema = &tool["inputSchema"];
            let disallow_additional = schema
                .get("additionalProperties")
                .and_then(Value::as_bool)
                == Some(false);
            let props: HashSet<String> = schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|obj| obj.keys().cloned().collect())
                .unwrap_or_default();
            out.insert(tool_name.to_string(), (props, disallow_additional));
        }
        out
    });

    let Some((allowed_props, disallow_additional)) = map.get(name) else {
        return Err(format!("unknown tool: {name}"));
    };
    let obj = Args::new(arguments).object()?;
    if *disallow_additional {
        if let Some(key) = obj.keys().find(|k| !allowed_props.contains(k.as_str())) {
            return Err(format!("unsupported argument: {key}"));
        }
    }
    Ok(())
}

pub(super) fn validate_tool_args(name: &str, arguments: &Value) -> Result<(), String> {
    validate_schema_additional_properties(name, arguments)?;
    let args = Args::new(arguments);

    match name {
        "list_chats" => {
            args.optional_bool("refresh")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
        }
        "create_chat" => {
            args.required_str_array("members")?;
            args.optional_str("topic")?;
        }
        "update_chat" => {
            args.reject_unknown(&["target", "topic"])?;
            args.required_str("target")?;
            args.required_text("topic")?;
        }
        "list_teams_channels" => {
            let refresh = args.optional_bool("refresh")?.unwrap_or(false);
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            let cursor = args.optional_str("cursor")?;
            if refresh && cursor.is_some() {
                return Err("cursor cannot be combined with refresh".to_string());
            }
        }
        "get_messages" => {
            args.reject_unknown(&["target", "limit", "before", "offline", "compact"])?;
            args.required_str("target")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            args.optional_str("before")?;
            args.optional_bool("offline")?;
            args.optional_bool("compact")?;
        }
        "get_thread" => {
            args.reject_unknown(&["target", "root_msg_id", "limit", "offline", "compact"])?;
            args.required_str("target")?;
            args.required_str("root_msg_id")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            args.optional_bool("offline")?;
            args.optional_bool("compact")?;
        }
        "send_message" => {
            args.reject_unknown(&[
                "target", "markdown", "subject", "mentions", "important", "compact",
            ])?;
            args.required_str("target")?;
            args.required_text("markdown")?;
            args.optional_str("subject")?;
            args.optional_mentions("mentions")?;
            args.optional_bool("important")?;
            args.optional_bool("compact")?;
        }
        "reply_message" => {
            args.reject_unknown(&[
                "target", "msg_id", "markdown", "mentions", "important", "compact",
            ])?;
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.required_text("markdown")?;
            args.optional_mentions("mentions")?;
            args.optional_bool("important")?;
            args.optional_bool("compact")?;
        }
        "send_file" => {
            args.reject_unknown(&[
                "target", "path", "caption", "thread_root_id", "inline", "compact",
            ])?;
            args.required_str("target")?;
            args.required_str("path")?;
            args.optional_str("caption")?;
            args.optional_str("thread_root_id")?;
            args.optional_bool("inline")?;
            args.optional_bool("compact")?;
        }
        "download_attachment" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.optional_usize("index")?;
            args.optional_str("out_dir")?;
            args.optional_str("thread_root_id")?;
        }
        "edit_message" => {
            args.reject_unknown(&[
                "target", "msg_id", "markdown", "find", "replace", "subject", "thread_root_id",
            ])?;
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.message_edit()?;
            args.optional_str("thread_root_id")?;
        }
        "delete_message" => {
            args.required_str("target")?;
            args.required_str("msg_id")?;
            args.required_true("confirm")?;
            args.optional_str("thread_root_id")?;
        }
        "search_messages" => {
            args.required_str("query")?;
            args.optional_str("target")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
        }
        "list_members" => {
            args.required_str("target")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(50),
                1,
                1_000,
            )?;
            args.optional_str("cursor")?;
            args.optional_bool("offline")?;
        }
        "search_users" => {
            args.reject_unknown(&["query", "limit"])?;
            args.required_str("query")?;
            bounds(
                "limit",
                args.optional_usize("limit")?.unwrap_or(10),
                1,
                1_000,
            )?;
        }
        "get_presence" => {
            args.optional_str_array("user_ids")?;
        }
        other => return Err(format!("unknown tool: {other}")),
    }

    Ok(())
}

pub(super) fn bounds(name: &str, value: usize, min: usize, max: usize) -> Result<usize, String> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{name} must be between {min} and {max}"))
    }
}

pub(super) struct Args<'a> {
    value: &'a Value,
}

impl<'a> Args<'a> {
    pub(super) fn new(value: &'a Value) -> Self {
        Self { value }
    }

    fn object(&self) -> Result<&'a serde_json::Map<String, Value>, String> {
        self.value
            .as_object()
            .ok_or_else(|| "arguments must be a JSON object".to_string())
    }

    fn reject_unknown(&self, allowed: &[&str]) -> Result<(), String> {
        if let Some(key) = self.object()?.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(format!("unsupported argument: {key}"));
        }
        Ok(())
    }

    pub(super) fn message_edit(&self) -> Result<cli::MessageEdit<'a>, String> {
        let text = |key| match self.object()?.get(key) {
            None => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.as_str())),
            Some(_) => Err(format!("{key} must be a string")),
        };
        let subject = text("subject")?;
        if subject.is_some() && self.object()?.contains_key("thread_root_id") {
            return Err("Subject edits require a channel root post.".to_string());
        }
        cli::MessageEdit::from_args(text("markdown")?, text("find")?, text("replace")?, subject)
            .map_err(format_error)
    }

    pub(super) fn optional_str(&self, key: &str) -> Result<Option<String>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(trimmed.to_string()))
                }
            }
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    pub(super) fn required_str(&self, key: &str) -> Result<String, String> {
        self.optional_str(key)?
            .ok_or_else(|| format!("missing required argument: {key}"))
    }

    pub(super) fn required_text(&self, key: &str) -> Result<String, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Err(format!("missing required argument: {key}")),
            Some(Value::String(value)) if !value.trim().is_empty() => Ok(value.clone()),
            Some(Value::String(_)) => Err(format!("{key} must not be empty")),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    pub(super) fn required_true(&self, key: &str) -> Result<(), String> {
        match self.object()?.get(key) {
            Some(Value::Bool(true)) => Ok(()),
            Some(Value::Bool(false)) => Err(format!("{key} must be true")),
            None | Some(Value::Null) => {
                Err(format!("missing required argument: {key} (must be true)"))
            }
            Some(_) => Err(format!("{key} must be a boolean true")),
        }
    }

    pub(super) fn optional_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(_) => Err(format!("{key} must be a boolean")),
        }
    }

    fn optional_i64(&self, key: &str) -> Result<Option<i64>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
                .map(Some)
                .ok_or_else(|| format!("{key} must be an integer")),
        }
    }

    pub(super) fn optional_usize(&self, key: &str) -> Result<Option<usize>, String> {
        match self.optional_i64(key)? {
            None => Ok(None),
            Some(v) => usize::try_from(v)
                .map(Some)
                .map_err(|_| format!("{key} must be a non-negative integer")),
        }
    }

    pub(super) fn optional_str_array(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    let s = item
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| format!("{key}[{i}] must be a non-empty string"))?;
                    out.push(s.to_string());
                }
                Ok(Some(out))
            }
            Some(_) => Err(format!("{key} must be an array of strings")),
        }
    }

    pub(super) fn required_str_array(&self, key: &str) -> Result<Vec<String>, String> {
        let items = self
            .optional_str_array(key)?
            .ok_or_else(|| format!("missing required argument: {key}"))?;
        if items.is_empty() {
            return Err(format!("{key} must not be empty"));
        }
        Ok(items)
    }

    pub(super) fn optional_mentions(&self, key: &str) -> Result<Vec<MentionInfo>, String> {
        match self.object()?.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(idx, item)| {
                    parse_mention_value(item, idx as i32)
                        .map_err(|e| format!("mentions[{idx}]: {e}"))
                })
                .collect(),
            Some(_) => Err(format!("{key} must be an array")),
        }
    }
}

fn parse_mention_value(value: &Value, default_id: i32) -> Result<MentionInfo, String> {
    if let Some(spec) = value.as_str() {
        return cli::parse_mention_spec(spec, default_id).map_err(format_error);
    }

    let obj = value
        .as_object()
        .ok_or_else(|| "mention must be a string or object".to_string())?;
    let user_id = obj
        .get("user_id")
        .or_else(|| obj.get("userId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "mention.user_id must be a non-empty string".to_string())?;
    let display_name = obj
        .get("display_name")
        .or_else(|| obj.get("displayName"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "mention.display_name must be a non-empty string".to_string())?;
    let id = obj
        .get("id")
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(default_id);

    Ok(MentionInfo {
        id,
        user_id: user_id.to_string(),
        display_name: display_name.to_string(),
    })
}
