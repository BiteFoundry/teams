//! Adaptive Card and O365 Connector Card JSON parser -> `RichBlock::Card`.

use bitefoundry_html::pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use serde_json::Value;

use crate::models::{AttachmentInfo, RichBlock, RichSpan, TextStyleFlags};

pub const ADAPTIVE_CARD_CONTENT_TYPE: &str = "application/vnd.microsoft.card.adaptive";
pub const CONNECTOR_CARD_CONTENT_TYPE: &str = "application/vnd.microsoft.teams.card.o365connector";
const MAX_CARD_DEPTH: usize = 16;

pub fn is_card_content_type(content_type: &str) -> bool {
    let ct = content_type.trim();
    ct.eq_ignore_ascii_case(ADAPTIVE_CARD_CONTENT_TYPE)
        || ct.eq_ignore_ascii_case(CONNECTOR_CARD_CONTENT_TYPE)
}

pub fn parse_card_attachment(att: &AttachmentInfo) -> Option<RichBlock> {
    let raw = att.content_json.as_deref()?.trim();
    if raw.is_empty() {
        return None;
    }
    let val: Value = serde_json::from_str(raw).ok()?;
    parse_card_value(&val, att.name.as_deref())
}

pub fn parse_card_value(val: &Value, fallback_name: Option<&str>) -> Option<RichBlock> {
    let obj = val.as_object()?;
    let is_connector = obj.contains_key("sections")
        || obj.contains_key("potentialAction")
        || obj
            .get("@type")
            .and_then(Value::as_str)
            .map(|t| t.eq_ignore_ascii_case("MessageCard"))
            .unwrap_or(false);

    if is_connector && !obj.contains_key("body") {
        return Some(parse_o365_connector(obj, fallback_name));
    }

    Some(parse_adaptive_card(obj, fallback_name))
}

fn parse_adaptive_card(obj: &serde_json::Map<String, Value>, fallback_name: Option<&str>) -> RichBlock {
    let mut title: Option<String> = obj
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut subtitle: Option<String> = None;
    let mut facts: Vec<(String, String)> = Vec::new();
    let mut body_lines: Vec<String> = Vec::new();
    let mut actions: Vec<(String, String)> = Vec::new();

    if let Some(body) = obj.get("body").and_then(Value::as_array) {
        for elem in body {
            collect_adaptive_element(
                elem,
                &mut title,
                &mut subtitle,
                &mut facts,
                &mut body_lines,
                &mut actions,
                0,
            );
        }
    }

    if let Some(card_actions) = obj.get("actions").and_then(Value::as_array) {
        collect_adaptive_actions(card_actions, &mut actions);
    }

    if title.is_none() {
        title = fallback_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    RichBlock::Card {
        title,
        subtitle,
        facts,
        body_lines,
        actions,
    }
}

fn collect_adaptive_element(
    elem: &Value,
    title: &mut Option<String>,
    subtitle: &mut Option<String>,
    facts: &mut Vec<(String, String)>,
    body_lines: &mut Vec<String>,
    actions: &mut Vec<(String, String)>,
    depth: usize,
) {
    if depth >= MAX_CARD_DEPTH {
        return;
    }
    let Some(obj) = elem.as_object() else {
        return;
    };
    let elem_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
    match elem_type {
        "TextBlock" => {
            let text = obj
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            if text.is_empty() {
                return;
            }
            let size = obj.get("size").and_then(Value::as_str).unwrap_or("");
            let weight = obj.get("weight").and_then(Value::as_str).unwrap_or("");
            let is_subtle = obj.get("isSubtle").and_then(Value::as_bool).unwrap_or(false);
            let is_heading = matches!(size, "Large" | "ExtraLarge" | "Medium")
                || weight.eq_ignore_ascii_case("Bolder");

            if title.is_none() && is_heading {
                *title = Some(text.to_string());
            } else if subtitle.is_none() && title.is_some() && is_subtle && body_lines.is_empty() {
                *subtitle = Some(text.to_string());
            } else {
                for line in text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        body_lines.push(trimmed.to_string());
                    }
                }
            }
        }
        "FactSet" => {
            if let Some(arr) = obj.get("facts").and_then(Value::as_array) {
                for fact in arr {
                    let k = fact
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    let v = fact
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !k.is_empty() || !v.is_empty() {
                        facts.push((k.to_string(), v.to_string()));
                    }
                }
            }
        }
        "Container" => {
            if let Some(items) = obj.get("items").and_then(Value::as_array) {
                for item in items {
                    collect_adaptive_element(
                        item,
                        title,
                        subtitle,
                        facts,
                        body_lines,
                        actions,
                        depth + 1,
                    );
                }
            }
        }
        "ColumnSet" => {
            if let Some(cols) = obj.get("columns").and_then(Value::as_array) {
                for col in cols {
                    if let Some(items) = col.get("items").and_then(Value::as_array) {
                        for item in items {
                            collect_adaptive_element(
                                item,
                                title,
                                subtitle,
                                facts,
                                body_lines,
                                actions,
                                depth + 1,
                            );
                        }
                    }
                }
            }
        }
        "ActionSet" => {
            if let Some(arr) = obj.get("actions").and_then(Value::as_array) {
                collect_adaptive_actions(arr, actions);
            }
        }
        "RichTextBlock" => {
            if let Some(inlines) = obj.get("inlines").and_then(Value::as_array) {
                let mut combined = String::new();
                for inline in inlines {
                    if let Some(s) = inline.as_str() {
                        combined.push_str(s);
                    } else if let Some(s) = inline.get("text").and_then(Value::as_str) {
                        combined.push_str(s);
                    }
                }
                let trimmed = combined.trim();
                if !trimmed.is_empty() {
                    body_lines.push(trimmed.to_string());
                }
            }
        }
        "Image" => {
            if let Some(alt) = obj
                .get("altText")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                body_lines.push(format!("[Image: {alt}]"));
            }
        }
        _ => {
            if let Some(items) = obj.get("items").and_then(Value::as_array) {
                for item in items {
                    collect_adaptive_element(
                        item,
                        title,
                        subtitle,
                        facts,
                        body_lines,
                        actions,
                        depth + 1,
                    );
                }
            }
        }
    }
}

fn collect_adaptive_actions(arr: &[Value], actions: &mut Vec<(String, String)>) {
    for action in arr {
        let Some(obj) = action.as_object() else {
            continue;
        };
        let action_type = obj.get("type").and_then(Value::as_str).unwrap_or("");
        let label = obj
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Action");

        match action_type {
            "Action.OpenUrl" => {
                let url = obj
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .unwrap_or("");
                actions.push((label.to_string(), url.to_string()));
            }
            "Action.Submit" => {
                let target = match obj.get("data") {
                    Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
                    Some(v) if !v.is_null() => v.to_string(),
                    _ => "submit".to_string(),
                };
                actions.push((label.to_string(), target));
            }
            "Action.ShowCard" => {
                actions.push((label.to_string(), "show_card".to_string()));
            }
            _ => {
                if let Some(url) = obj.get("url").and_then(Value::as_str) {
                    actions.push((label.to_string(), url.trim().to_string()));
                } else {
                    actions.push((label.to_string(), action_type.to_string()));
                }
            }
        }
    }
}

fn parse_o365_connector(
    obj: &serde_json::Map<String, Value>,
    fallback_name: Option<&str>,
) -> RichBlock {
    let mut title = obj
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut subtitle = obj
        .get("summary")
        .or_else(|| obj.get("subtitle"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let mut facts = Vec::new();
    let mut body_lines = Vec::new();
    let mut actions = Vec::new();

    if let Some(text) = obj
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        for line in text.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                body_lines.push(trimmed.to_string());
            }
        }
    }

    if let Some(sections) = obj.get("sections").and_then(Value::as_array) {
        for sec in sections {
            let Some(sec_obj) = sec.as_object() else {
                continue;
            };
            if let Some(sec_title) = sec_obj
                .get("activityTitle")
                .or_else(|| sec_obj.get("title"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if title.is_none() {
                    title = Some(sec_title.to_string());
                } else {
                    body_lines.push(sec_title.to_string());
                }
            }
            if let Some(sec_sub) = sec_obj
                .get("activitySubtitle")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if subtitle.is_none() {
                    subtitle = Some(sec_sub.to_string());
                } else {
                    body_lines.push(sec_sub.to_string());
                }
            }
            if let Some(sec_text) = sec_obj
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                for line in sec_text.lines() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        body_lines.push(trimmed.to_string());
                    }
                }
            }
            if let Some(sec_facts) = sec_obj.get("facts").and_then(Value::as_array) {
                for f in sec_facts {
                    let k = f
                        .get("name")
                        .or_else(|| f.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    let v = f
                        .get("value")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim();
                    if !k.is_empty() || !v.is_empty() {
                        facts.push((k.to_string(), v.to_string()));
                    }
                }
            }
            if let Some(sec_actions) = sec_obj.get("potentialAction").and_then(Value::as_array) {
                collect_connector_actions(sec_actions, &mut actions);
            }
        }
    }

    if let Some(top_actions) = obj.get("potentialAction").and_then(Value::as_array) {
        collect_connector_actions(top_actions, &mut actions);
    }

    if title.is_none() {
        title = fallback_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }

    RichBlock::Card {
        title,
        subtitle,
        facts,
        body_lines,
        actions,
    }
}

fn collect_connector_actions(arr: &[Value], actions: &mut Vec<(String, String)>) {
    for action in arr {
        let Some(obj) = action.as_object() else {
            continue;
        };
        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Action");
        let uri = obj
            .get("targets")
            .and_then(Value::as_array)
            .and_then(|targets| targets.first())
            .and_then(|t| t.get("uri"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        actions.push((name.to_string(), uri.to_string()));
    }
}

fn push_card_span(spans: &mut Vec<RichSpan>, text: &str, style: &TextStyleFlags) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if &last.style == style {
            last.text.push_str(text);
            return;
        }
    }
    spans.push(RichSpan {
        text: text.to_string(),
        style: style.clone(),
    });
}

fn ensure_card_newline(spans: &mut Vec<RichSpan>) {
    if !spans.is_empty() && !spans.last().is_some_and(|s| s.text.ends_with('\n')) {
        push_card_span(spans, "\n", &TextStyleFlags::default());
    }
}

/// Parses Adaptive Card / O365 Connector Card Markdown (`TextBlock`, `FactSet`, etc.)
/// into styled [`RichSpan`]s via `bitefoundry_html::pulldown_cmark`, preserving explicit `\n` breaks.
pub fn parse_card_markdown_spans(input: &str) -> Vec<RichSpan> {
    if input.trim().is_empty() {
        return Vec::new();
    }

    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_TABLES);

    let default_style = TextStyleFlags::default();
    let mut style_stack: Vec<TextStyleFlags> = vec![default_style.clone()];
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut quote_depth: usize = 0;
    let mut in_table_head = false;
    let mut table_cell_idx: usize = 0;
    let mut spans: Vec<RichSpan> = Vec::new();

    for event in Parser::new_ext(input, opts) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    if !spans.is_empty() && !spans.last().is_some_and(|s| s.text.ends_with('\n')) {
                        if list_stack.is_empty() || !spans.last().is_some_and(|s| s.text.ends_with("• ") || s.text.ends_with(". ")) {
                            push_card_span(&mut spans, "\n", &default_style);
                            if quote_depth > 0 {
                                push_card_span(&mut spans, "│ ", &default_style);
                            }
                        }
                    }
                }
                Tag::Heading { .. } => {
                    ensure_card_newline(&mut spans);
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.bold = true;
                    style_stack.push(next);
                }
                Tag::BlockQuote(_) => {
                    ensure_card_newline(&mut spans);
                    quote_depth += 1;
                    push_card_span(&mut spans, "│ ", &default_style);
                }
                Tag::List(start) => {
                    list_stack.push(start);
                }
                Tag::Item => {
                    ensure_card_newline(&mut spans);
                    let depth = list_stack.len().saturating_sub(1);
                    if depth > 0 {
                        push_card_span(&mut spans, &"  ".repeat(depth), &default_style);
                    }
                    match list_stack.last_mut() {
                        Some(Some(num)) => {
                            let marker = format!("{num}. ");
                            *num += 1;
                            push_card_span(&mut spans, &marker, &default_style);
                        }
                        _ => {
                            push_card_span(&mut spans, "• ", &default_style);
                        }
                    }
                }
                Tag::Strong => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.bold = true;
                    style_stack.push(next);
                }
                Tag::Emphasis => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.italic = true;
                    style_stack.push(next);
                }
                Tag::Strikethrough => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.strike = true;
                    style_stack.push(next);
                }
                Tag::Link { dest_url, .. } => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    let url = dest_url.trim();
                    if !url.is_empty() {
                        next.link_url = Some(url.to_string());
                    }
                    style_stack.push(next);
                }
                Tag::CodeBlock(_) => {
                    ensure_card_newline(&mut spans);
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.code = true;
                    style_stack.push(next);
                }
                Tag::TableHead => {
                    ensure_card_newline(&mut spans);
                    in_table_head = true;
                    table_cell_idx = 0;
                }
                Tag::TableRow => {
                    ensure_card_newline(&mut spans);
                    in_table_head = false;
                    table_cell_idx = 0;
                }
                Tag::TableCell => {
                    if table_cell_idx > 0 {
                        push_card_span(&mut spans, " │ ", &default_style);
                    }
                    table_cell_idx += 1;
                    if in_table_head {
                        let mut next = style_stack.last().cloned().unwrap_or_default();
                        next.bold = true;
                        style_stack.push(next);
                    }
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Heading(_)
                | TagEnd::Strong
                | TagEnd::Emphasis
                | TagEnd::Strikethrough
                | TagEnd::Link => {
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some(last) = spans.last_mut() {
                        let trimmed = last.text.trim_end_matches(['\r', '\n']);
                        if trimmed.len() < last.text.len() {
                            last.text.truncate(trimmed.len());
                        }
                        if last.text.is_empty() {
                            spans.pop();
                        }
                    }
                    if style_stack.len() > 1 {
                        style_stack.pop();
                    }
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                TagEnd::TableHead => {
                    in_table_head = false;
                }
                TagEnd::TableCell => {
                    if in_table_head && style_stack.len() > 1 {
                        style_stack.pop();
                    }
                }
                _ => {}
            },
            Event::Text(text) => {
                let style = style_stack.last().unwrap_or(&default_style);
                push_card_span(&mut spans, &text, style);
            }
            Event::Code(code) => {
                let mut style = style_stack.last().cloned().unwrap_or_default();
                style.code = true;
                push_card_span(&mut spans, &code, &style);
            }
            Event::SoftBreak | Event::HardBreak => {
                push_card_span(&mut spans, "\n", &default_style);
                if quote_depth > 0 {
                    push_card_span(&mut spans, "│ ", &default_style);
                }
            }
            Event::TaskListMarker(checked) => {
                let style = style_stack.last().unwrap_or(&default_style);
                push_card_span(&mut spans, if checked { "[x] " } else { "[ ] " }, style);
            }
            Event::Rule => {
                ensure_card_newline(&mut spans);
                push_card_span(&mut spans, "────────────────", &default_style);
            }
            Event::InlineHtml(raw) | Event::Html(raw) => {
                let lower = raw.trim().to_ascii_lowercase();
                if lower == "<br>" || lower == "<br/>" || lower == "<br />" {
                    push_card_span(&mut spans, "\n", &default_style);
                }
            }
            _ => {}
        }
    }

    while let Some(last) = spans.last_mut() {
        let trimmed_len = last.text.trim_end_matches(['\r', '\n']).len();
        if trimmed_len == 0 {
            spans.pop();
        } else {
            if trimmed_len < last.text.len() {
                last.text.truncate(trimmed_len);
            }
            break;
        }
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_card_content_type_case_insensitive() {
        assert!(is_card_content_type("Application/VND.Microsoft.Card.Adaptive"));
        assert!(is_card_content_type("  application/vnd.microsoft.teams.card.o365connector "));
        assert!(!is_card_content_type("reference"));
    }

    #[test]
    fn test_collect_adaptive_element_respects_max_depth() {
        let mut node = serde_json::json!({
            "type": "TextBlock",
            "text": "Too deep"
        });
        for _ in 0..(MAX_CARD_DEPTH + 4) {
            node = serde_json::json!({
                "type": "Container",
                "items": [node]
            });
        }
        let card = serde_json::json!({
            "type": "AdaptiveCard",
            "body": [node]
        });
        let parsed = parse_card_value(&card, Some("Depth Test")).unwrap();
        if let RichBlock::Card { body_lines, .. } = parsed {
            assert!(body_lines.is_empty());
        } else {
            panic!("expected RichBlock::Card");
        }
    }
}
