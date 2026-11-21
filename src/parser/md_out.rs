//! Outgoing Markdown + mentions + attachments + quote reply -> safe Microsoft Teams HTML compiler.

use crate::models::{
    InlineImageUpload, MentionInfo, OutgoingMessage, QuotedReply, UploadedReferenceAttachment,
};

pub fn compile_outgoing_markdown(
    markdown: &str,
    subject: Option<String>,
    mentions: Vec<MentionInfo>,
    attachments: Vec<UploadedReferenceAttachment>,
    inline_images: Vec<InlineImageUpload>,
    quote_reply_to: Option<QuotedReply>,
) -> OutgoingMessage {
    let mut html = String::new();

    if let Some(q) = &quote_reply_to {
        html.push_str(&format!(
            "<blockquote itemtype=\"http://schema.skype.com/Reply\" itemid=\"{}\"><strong>{}</strong><br>{}</blockquote>",
            escape_html_attr(&q.msg_id),
            escape_html(&q.author_name),
            escape_html(&q.preview_plain)
        ));
    }

    let body = render_markdown_to_html(markdown, &mentions);
    html.push_str(&body);

    for img in &inline_images {
        html.push_str(&format!(
            "<img src=\"../hostedContents/{}/$value\">",
            escape_html_attr(&img.temp_id)
        ));
    }

    for att in &attachments {
        html.push_str(&format!(
            "<attachment id=\"{}\"></attachment>",
            escape_html_attr(&att.id)
        ));
    }

    OutgoingMessage {
        subject,
        body_html: html,
        mentions,
        attachments,
        inline_images,
        importance: None,
        quote_reply_to,
    }
}

fn render_markdown_to_html(markdown: &str, mentions: &[MentionInfo]) -> String {
    let normalized: std::borrow::Cow<'_, str> = if markdown.contains('\r') {
        std::borrow::Cow::Owned(markdown.replace("\r\n", "\n"))
    } else {
        std::borrow::Cow::Borrowed(markdown)
    };
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut blocks: Vec<String> = Vec::new();
    let mut i = 0;

    let mut sorted_mentions: Vec<&MentionInfo> = mentions.iter().collect();
    sorted_mentions.sort_by(|a, b| b.display_name.len().cmp(&a.display_name.len()));

    while i < lines.len() {
        let line = lines[i];

        // Fenced code block ```lang ... ```
        if let Some(fence_rest) = line.trim_start().strip_prefix("```") {
            let lang = fence_rest.trim();
            i += 1;
            let mut code_lines = Vec::new();
            while i < lines.len() {
                if lines[i].trim_start().starts_with("```") {
                    i += 1;
                    break;
                }
                code_lines.push(lines[i]);
                i += 1;
            }
            let code_escaped = escape_html(&code_lines.join("\n"));
            if lang.is_empty() {
                blocks.push(format!("<pre><code>{code_escaped}</code></pre>"));
            } else {
                blocks.push(format!(
                    "<pre><code class=\"language-{}\">{code_escaped}</code></pre>",
                    escape_html_attr(lang)
                ));
            }
            continue;
        }

        // Blank line separator
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        // Blockquote lines starting with "> " (or ">")
        if line.starts_with("> ") || line == ">" {
            let mut quote_lines = Vec::new();
            while i < lines.len() && (lines[i].starts_with("> ") || lines[i] == ">") {
                let content = lines[i]
                    .strip_prefix("> ")
                    .or_else(|| lines[i].strip_prefix('>'))
                    .unwrap_or("");
                quote_lines.push(render_inline(content, &sorted_mentions));
                i += 1;
            }
            blocks.push(format!("<blockquote>{}</blockquote>", quote_lines.join("<br>")));
            continue;
        }

        // Unordered list (- item or * item)
        if parse_unordered_list_item(line).is_some() {
            let mut items = Vec::new();
            while i < lines.len() {
                if let Some(item_text) = parse_unordered_list_item(lines[i]) {
                    items.push(format!(
                        "<li>{}</li>",
                        render_inline(item_text, &sorted_mentions)
                    ));
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(format!("<ul>{}</ul>", items.join("")));
            continue;
        }

        // Paragraph until blank line, fence, blockquote, or unordered list
        let mut para_lines = Vec::new();
        while i < lines.len() {
            let cur = lines[i];
            if cur.trim().is_empty()
                || cur.trim_start().starts_with("```")
                || cur.starts_with("> ")
                || cur == ">"
                || parse_unordered_list_item(cur).is_some()
            {
                break;
            }
            para_lines.push(render_inline(cur, &sorted_mentions));
            i += 1;
        }
        if !para_lines.is_empty() {
            blocks.push(format!("<p>{}</p>", para_lines.join("<br>")));
        }
    }

    blocks.join("")
}

fn parse_unordered_list_item(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .map(str::trim)
}

fn render_inline(input: &str, mentions: &[&MentionInfo]) -> String {
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut no_closing_backtick = false;
    let mut no_closing_bracket = false;
    let mut no_closing_double_star = false;
    let mut no_closing_tilde = false;
    let mut no_closing_star = false;

    while i < input.len() {
        let rest = &input[i..];

        // Inline code: `...`
        if !no_closing_backtick {
            if let Some(after_tick) = rest.strip_prefix('`') {
                if let Some(end_idx) = after_tick.find('`') {
                    let code = &after_tick[..end_idx];
                    out.push_str("<code>");
                    out.push_str(&escape_html(code));
                    out.push_str("</code>");
                    i += 1 + end_idx + 1;
                    continue;
                } else {
                    no_closing_backtick = true;
                }
            }
        }

        // Mention: @DisplayName
        if rest.starts_with('@') && is_mention_boundary(input, i) {
            if let Some(m) = mentions.iter().find(|m| {
                !m.display_name.is_empty()
                    && rest[1..].starts_with(&m.display_name)
                    && is_mention_end_boundary(&rest[1 + m.display_name.len()..])
            }) {
                out.push_str(&format!(
                    "<at id=\"{}\">{}</at>",
                    m.id,
                    escape_html(&m.display_name)
                ));
                i += 1 + m.display_name.len();
                continue;
            }
        }

        // Markdown link: [label](url)
        if !no_closing_bracket && rest.starts_with('[') {
            match parse_markdown_link(rest) {
                Some((label, url, consumed)) => {
                    if is_safe_url(url) {
                        out.push_str(&format!(
                            "<a href=\"{}\">{}</a>",
                            escape_html_attr(url),
                            render_inline(label, mentions)
                        ));
                        i += consumed;
                        continue;
                    }
                }
                None => {
                    no_closing_bracket = true;
                }
            }
        }

        // Bold: **...**
        if !no_closing_double_star {
            if let Some(after_stars) = rest.strip_prefix("**") {
                match after_stars.find("**") {
                    Some(end_idx) => {
                        if end_idx > 0 {
                            let inner = &after_stars[..end_idx];
                            out.push_str("<strong>");
                            out.push_str(&render_inline(inner, mentions));
                            out.push_str("</strong>");
                            i += 2 + end_idx + 2;
                            continue;
                        }
                    }
                    None => {
                        no_closing_double_star = true;
                    }
                }
            }
        }

        // Strikethrough: ~~...~~
        if !no_closing_tilde {
            if let Some(after_tilde) = rest.strip_prefix("~~") {
                match after_tilde.find("~~") {
                    Some(end_idx) => {
                        if end_idx > 0 {
                            let inner = &after_tilde[..end_idx];
                            out.push_str("<s>");
                            out.push_str(&render_inline(inner, mentions));
                            out.push_str("</s>");
                            i += 2 + end_idx + 2;
                            continue;
                        }
                    }
                    None => {
                        no_closing_tilde = true;
                    }
                }
            }
        }

        // Italic: *...* (single asterisk, not **)
        if !no_closing_star && rest.starts_with('*') && !rest.starts_with("**") {
            match find_closing_single_asterisk(&rest[1..]) {
                Some(end_idx) => {
                    if end_idx > 0 {
                        let inner = &rest[1..1 + end_idx];
                        out.push_str("<em>");
                        out.push_str(&render_inline(inner, mentions));
                        out.push_str("</em>");
                        i += 1 + end_idx + 1;
                        continue;
                    }
                }
                None => {
                    no_closing_star = true;
                }
            }
        }

        // Escape next character literally and safely
        let ch = rest.chars().next().unwrap();
        push_escaped_char(&mut out, ch);
        i += ch.len_utf8();
    }

    out
}

fn is_mention_boundary(full: &str, at_byte_idx: usize) -> bool {
    if at_byte_idx == 0 {
        return true;
    }
    full[..at_byte_idx]
        .chars()
        .next_back()
        .map(|c| !c.is_alphanumeric())
        .unwrap_or(true)
}

fn is_mention_end_boundary(after: &str) -> bool {
    match after.chars().next() {
        None => true,
        Some(c) => !c.is_alphanumeric() && c != '_',
    }
}

fn parse_markdown_link(rest: &str) -> Option<(&str, &str, usize)> {
    let after_open = rest.strip_prefix('[')?;
    let close_bracket = after_open.find("](")?;
    let label = &after_open[..close_bracket];
    let after_mid = &after_open[close_bracket + 2..];
    let close_paren = after_mid.find(')')?;
    let url = after_mid[..close_paren].trim();
    if url.is_empty() {
        return None;
    }
    let consumed = 1 + close_bracket + 2 + close_paren + 1;
    Some((label, url, consumed))
}

fn is_safe_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("mailto:")
}

fn find_closing_single_asterisk(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'*' {
            let next_is_star = idx + 1 < bytes.len() && bytes[idx + 1] == b'*';
            if !next_is_star {
                return Some(idx);
            }
            idx += 2;
            continue;
        }
        idx += 1;
    }
    None
}

fn push_escaped_char(out: &mut String, ch: char) {
    match ch {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&#39;"),
        _ => out.push(ch),
    }
}

pub fn escape_html(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        push_escaped_char(&mut out, ch);
    }
    out
}

pub fn escape_html_attr(input: &str) -> String {
    escape_html(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_inline_linear_on_unmatched_delimiters_and_unsafe_links() {
        let many_ticks = "` ".repeat(2000);
        let many_stars = "* ".repeat(2000);
        let many_double = "** ".repeat(2000);
        let many_tilde = "~~ ".repeat(2000);
        let many_brackets = "[ ".repeat(2000);

        let payload = format!(
            "{many_ticks}\n\n{many_stars}\n\n{many_double}\n\n{many_tilde}\n\n{many_brackets}\n\n[bad](javascript:alert(1)) [ok](https://example.com)"
        );
        let html = render_markdown_to_html(&payload, &[]);
        assert!(html.contains("<a href=\"https://example.com\">ok</a>"));
        assert!(!html.contains("<a href=\"javascript:"));
    }

    #[test]
    fn test_render_markdown_lists_mentions_blank_line_and_code_block() {
        let mentions = vec![MentionInfo {
            id: 0,
            user_id: "u1".to_string(),
            display_name: "Alice".to_string(),
        }];

        let html = render_markdown_to_html("- first\n- second", &[]);
        assert_eq!(html, "<ul><li>first</li><li>second</li></ul>");

        let with_inline = render_markdown_to_html(
            "- **first** @Alice\n- second [docs](https://example.com) `v1`\n\nAfter list\n\n```\n- code item 1\n- code item 2\n```",
            &mentions,
        );
        assert_eq!(
            with_inline,
            "<ul><li><strong>first</strong> <at id=\"0\">Alice</at></li><li>second <a href=\"https://example.com\">docs</a> <code>v1</code></li></ul><p>After list</p><pre><code>- code item 1\n- code item 2</code></pre>"
        );

        let numbered_preserved = render_markdown_to_html("7. keep seven\n8. keep eight", &[]);
        assert_eq!(numbered_preserved, "<p>7. keep seven<br>8. keep eight</p>");
        assert!(!numbered_preserved.contains("<ol>"));

        let split_lists = render_markdown_to_html("- alpha\n- beta\n\n- gamma", &[]);
        assert_eq!(
            split_lists,
            "<ul><li>alpha</li><li>beta</li></ul><ul><li>gamma</li></ul>"
        );

        let intro_then_list = render_markdown_to_html("Summary:\n* one\n* two", &[]);
        assert_eq!(
            intro_then_list,
            "<p>Summary:</p><ul><li>one</li><li>two</li></ul>"
        );

        let non_list_hyphens = render_markdown_to_html("---\n-item\n--dry-run", &[]);
        assert_eq!(non_list_hyphens, "<p>---<br>-item<br>--dry-run</p>");
    }
}
