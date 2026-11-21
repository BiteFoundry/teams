//! Bidirectional parser between Teams HTML / Adaptive Cards, Plain Text / Markdown, and Rich Terminal AST.

pub mod card_in;
pub(crate) mod edit_text;
pub mod html_in;
pub mod md_out;

use crate::models::{
    AttachmentInfo, ConversationTarget, InlineImageUpload, MentionInfo, OutgoingMessage,
    QuotedReply, RichBlock, UploadedReferenceAttachment,
};

pub use html_in::parse_message_blocks_arc;

/// Extracts `AttachmentInfo` entries for inline `<img src="...">` elements in `body_html`
/// not already represented in `existing`.
pub fn extract_inline_image_attachments(
    body_html: &str,
    existing: &[AttachmentInfo],
    conv_key: &str,
    msg_id: &str,
    parent_msg_id: Option<&str>,
) -> Vec<AttachmentInfo> {
    if !body_html
        .as_bytes()
        .windows(4)
        .any(|w| w.eq_ignore_ascii_case(b"<img"))
    {
        return Vec::new();
    }

    let blocks = parse_message_blocks_arc(body_html, existing, &[]);
    let mut out = Vec::new();

    for block in blocks.iter() {
        let RichBlock::InlineImageChip { src, index } = block else {
            continue;
        };

        let resolved_url = resolve_inline_image_url(src, conv_key, msg_id, parent_msg_id);
        let hosted_id = html_in::extract_hosted_content_id(&resolved_url);

        let id = match hosted_id {
            Some(seg) => format!("hosted:{seg}"),
            None => format!("inline-img-{}", index + 1),
        };

        let is_duplicate = |att: &AttachmentInfo| {
            att.content_url
                .as_deref()
                .is_some_and(|u| u == src || u == resolved_url)
                || att.id == id
                || hosted_id.is_some_and(|hid| att.id == hid || att.id == format!("hosted:{hid}"))
        };

        if existing.iter().any(is_duplicate) || out.iter().any(is_duplicate) {
            continue;
        }

        let content_type = if hosted_id.is_some() {
            "hostedContent".to_string()
        } else {
            "image/png".to_string()
        };

        out.push(AttachmentInfo {
            id,
            content_type,
            content_url: Some(resolved_url),
            name: Some(format!("inline-image-{}.png", index + 1)),
            content_json: None,
            thumbnail_url: None,
        });
    }

    out
}

fn resolve_inline_image_url(
    src: &str,
    conv_key: &str,
    msg_id: &str,
    parent_msg_id: Option<&str>,
) -> String {
    let tail = src
        .strip_prefix("../")
        .or_else(|| src.strip_prefix("./"))
        .unwrap_or(src);

    if tail.starts_with("hostedContents/") && !msg_id.is_empty() {
        if let Some(target) = ConversationTarget::from_conv_key(conv_key) {
            return match target {
                ConversationTarget::Chat { chat_id } => format!(
                    "/chats/{}/messages/{}/{}",
                    urlencoding::encode(&chat_id),
                    urlencoding::encode(msg_id),
                    tail
                ),
                ConversationTarget::Channel {
                    team_id,
                    channel_id,
                } => {
                    if let Some(root_id) =
                        parent_msg_id.filter(|p| !p.is_empty() && *p != msg_id)
                    {
                        format!(
                            "/teams/{}/channels/{}/messages/{}/replies/{}/{}",
                            urlencoding::encode(&team_id),
                            urlencoding::encode(&channel_id),
                            urlencoding::encode(root_id),
                            urlencoding::encode(msg_id),
                            tail
                        )
                    } else {
                        format!(
                            "/teams/{}/channels/{}/messages/{}/{}",
                            urlencoding::encode(&team_id),
                            urlencoding::encode(&channel_id),
                            urlencoding::encode(msg_id),
                            tail
                        )
                    }
                }
            };
        }
    }

    src.to_string()
}

/// Strips HTML tags and formats Adaptive Cards into clean plain text for previews & FTS5 indexing.
pub fn html_and_cards_to_plain(html: &str, attachments: &[AttachmentInfo]) -> String {
    let blocks = html_in::parse_message_blocks_no_cache(html, attachments, &[]);
    html_in::blocks_to_plain(&blocks)
}

/// Converts a Teams HTML message + attachments into clean GitHub-flavored Markdown (used by `export --format md`).
pub fn html_and_cards_to_markdown(html: &str, attachments: &[AttachmentInfo]) -> String {
    let blocks = html_in::parse_message_blocks_no_cache(html, attachments, &[]);
    html_in::blocks_to_markdown(&blocks)
}

/// Parses Teams HTML + Adaptive Cards into structured `RichBlock`s for styled `ratatui` rendering.
pub fn parse_message_blocks(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> Vec<RichBlock> {
    html_in::parse_message_blocks(html, attachments, mentions)
}

/// Compiles user Markdown input + `@mention` list + uploaded reference attachments + inline images
/// + optional quote reply into a valid `OutgoingMessage` ready for Microsoft Graph.
pub fn compile_outgoing_markdown(
    markdown: &str,
    subject: Option<String>,
    mentions: Vec<MentionInfo>,
    attachments: Vec<UploadedReferenceAttachment>,
    inline_images: Vec<InlineImageUpload>,
    quote_reply_to: Option<QuotedReply>,
) -> OutgoingMessage {
    md_out::compile_outgoing_markdown(
        markdown,
        subject,
        mentions,
        attachments,
        inline_images,
        quote_reply_to,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_html_to_blocks_plain_and_markdown() {
        let html = r#"
            <blockquote itemtype="http://schema.skype.com/Reply" itemid="100"><strong>Bob</strong><br>Previous question</blockquote>
            <p>Hello <at id="0">Alice</at>, check <b>deploy</b> <i>schedule</i> <s>cancel</s> <u>notice</u> <code>v1.2</code> <a href="https://example.com">link</a>!</p>
            <h2>Checklist</h2>
            <ul><li>Verify build</li><li>Tests passing</li></ul>
            <pre><code class="language-rust">fn main() { println!("hi"); }</code></pre>
            <img src="https://graph.microsoft.com/img1">
            <attachment id="att-1"></attachment>
        "#;
        let attachments = vec![
            AttachmentInfo {
                id: "att-1".to_string(),
                content_type: "reference".to_string(),
                content_url: Some("https://contoso.sharepoint.com/spec.pdf".to_string()),
                name: Some("spec.pdf".to_string()),
                content_json: None,
                thumbnail_url: None,
            },
            AttachmentInfo {
                id: "card-unreferenced".to_string(),
                content_type: "application/vnd.microsoft.card.adaptive".to_string(),
                content_url: None,
                name: Some("Deploy alert".to_string()),
                content_json: Some(
                    r#"{
                        "type": "AdaptiveCard",
                        "body": [
                            {"type": "TextBlock", "text": "Release Summary", "size": "Large", "weight": "Bolder"},
                            {"type": "FactSet", "facts": [{"title": "Environment", "value": "Production"}]},
                            {"type": "ColumnSet", "columns": [
                                {"type": "Column", "items": [{"type": "TextBlock", "text": "Column A"}]},
                                {"type": "Column", "items": [{"type": "TextBlock", "text": "Column B"}]}
                            ]},
                            {"type": "Container", "items": [{"type": "TextBlock", "text": "Deployment succeeded"}]}
                        ],
                        "actions": [
                            {"type": "Action.OpenUrl", "title": "Dashboard", "url": "https://dash.example.com"}
                        ]
                    }"#
                    .to_string(),
                ),
                thumbnail_url: None,
            },
        ];
        let mentions = vec![MentionInfo {
            id: 0,
            user_id: "u1".to_string(),
            display_name: "Alice".to_string(),
        }];

        let blocks = parse_message_blocks(html, &attachments, &mentions);
        assert!(matches!(blocks[0], RichBlock::Quote(_)));
        assert!(matches!(blocks[1], RichBlock::Paragraph(_)));
        assert!(matches!(blocks[2], RichBlock::Heading { level: 2, .. }));
        assert!(matches!(blocks[3], RichBlock::BulletList(_)));
        assert!(matches!(
            blocks[4],
            RichBlock::CodeBlock {
                lang: Some(_),
                ..
            }
        ));
        assert!(matches!(
            blocks[5],
            RichBlock::InlineImageChip { index: 0, .. }
        ));
        assert!(matches!(blocks[6], RichBlock::AttachmentChip { .. }));
        assert!(matches!(blocks[7], RichBlock::Card { .. }));

        if let RichBlock::Card {
            title,
            facts,
            body_lines,
            actions,
            ..
        } = &blocks[7]
        {
            assert_eq!(title.as_deref(), Some("Release Summary"));
            assert_eq!(
                facts,
                &vec![("Environment".to_string(), "Production".to_string())]
            );
            assert_eq!(
                body_lines,
                &vec![
                    "Column A".to_string(),
                    "Column B".to_string(),
                    "Deployment succeeded".to_string()
                ]
            );
            assert_eq!(
                actions,
                &vec![("Dashboard".to_string(), "https://dash.example.com".to_string())]
            );
        }

        let plain = html_and_cards_to_plain(html, &attachments);
        assert!(plain.contains("@Alice"));
        assert!(plain.contains("Release Summary"));
        assert!(plain.contains("Environment: Production"));
        assert!(plain.contains("[Attachment: spec.pdf]"));
        assert!(!plain.contains("schema.skype.com"));

        let md = html_and_cards_to_markdown(html, &attachments);
        assert!(md.contains("**deploy**"));
        assert!(md.contains("*schedule*"));
        assert!(md.contains("~~cancel~~"));
        assert!(md.contains("[link](https://example.com)"));
        assert!(md.contains("```rust"));
        assert!(md.contains("## Checklist"));
    }

    #[test]
    fn test_o365_connector_card_and_columnset() {
        let att = AttachmentInfo {
            id: "c1".to_string(),
            content_type: "application/vnd.microsoft.teams.card.o365connector".to_string(),
            content_url: None,
            name: None,
            content_json: Some(
                r#"{
                    "@type": "MessageCard",
                    "title": "CI build failed",
                    "summary": "main branch",
                    "sections": [{
                        "activityTitle": "GitHub Actions",
                        "facts": [{"name": "Commit", "value": "abc1234"}],
                        "text": "cargo test failed"
                    }],
                    "potentialAction": [{
                        "name": "View logs",
                        "targets": [{"uri": "https://ci.example.com/1"}]
                    }]
                }"#
                .to_string(),
            ),
            thumbnail_url: None,
        };
        let blocks = parse_message_blocks("", &[att], &[]);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            RichBlock::Card {
                title,
                subtitle,
                facts,
                body_lines,
                actions,
            } => {
                assert_eq!(title.as_deref(), Some("CI build failed"));
                assert_eq!(subtitle.as_deref(), Some("main branch"));
                assert_eq!(facts, &vec![("Commit".to_string(), "abc1234".to_string())]);
                assert!(body_lines.contains(&"cargo test failed".to_string()));
                assert_eq!(
                    actions,
                    &vec![("View logs".to_string(), "https://ci.example.com/1".to_string())]
                );
            }
            other => panic!("expected Card, got {other:?}"),
        }
    }

    #[test]
    fn test_compile_outgoing_markdown_escaping_and_features() {
        let md = "Hello @Alice! `<script>alert(1)</script>`\n**bold** *italic* ~~strike~~ [docs](https://example.com?a=1&b=2)\n\n> quote line 1\n> quote line 2\n\n```rust\nlet x = 1 < 2;\n```\n\n<raw-html-must-escape>";
        let mentions = vec![MentionInfo {
            id: 0,
            user_id: "u1".to_string(),
            display_name: "Alice".to_string(),
        }];
        let attachments = vec![UploadedReferenceAttachment {
            id: "guid-1".to_string(),
            name: "report.pdf".to_string(),
            content_url: "https://sharepoint/report.pdf".to_string(),
            size: 1024,
            drive_item_id: None,
        }];
        let inline_images = vec![InlineImageUpload {
            temp_id: "1".to_string(),
            mime_type: "image/png".to_string(),
            bytes: vec![1, 2, 3],
            file_name: Some("shot.png".to_string()),
        }];
        let quote = Some(QuotedReply {
            msg_id: "m99".to_string(),
            author_name: "Bob <Admin>".to_string(),
            preview_plain: "original & content".to_string(),
        });

        let out = compile_outgoing_markdown(
            md,
            Some("Subject".to_string()),
            mentions,
            attachments,
            inline_images,
            quote,
        );

        assert!(out
            .body_html
            .starts_with("<blockquote itemtype=\"http://schema.skype.com/Reply\" itemid=\"m99\"><strong>Bob &lt;Admin&gt;</strong><br>original &amp; content</blockquote>"));
        assert!(out.body_html.contains("<at id=\"0\">Alice</at>"));
        assert!(out
            .body_html
            .contains("<code>&lt;script&gt;alert(1)&lt;/script&gt;</code>"));
        assert!(out.body_html.contains("<strong>bold</strong>"));
        assert!(out.body_html.contains("<em>italic</em>"));
        assert!(out.body_html.contains("<s>strike</s>"));
        assert!(out
            .body_html
            .contains("<a href=\"https://example.com?a=1&amp;b=2\">docs</a>"));
        assert!(out
            .body_html
            .contains("<blockquote>quote line 1<br>quote line 2</blockquote>"));
        assert!(out
            .body_html
            .contains("<pre><code class=\"language-rust\">let x = 1 &lt; 2;</code></pre>"));
        assert!(out.body_html.contains("&lt;raw-html-must-escape&gt;"));
        assert!(!out.body_html.contains("<raw-html-must-escape>"));
        assert!(out
            .body_html
            .contains("<img src=\"../hostedContents/1/$value\">"));
        assert!(out
            .body_html
            .contains("<attachment id=\"guid-1\"></attachment>"));
    }

    #[test]
    fn test_parse_message_blocks_arc_fast_path_and_styled_markdown() {
        let empty = parse_message_blocks_arc("   \n\t ", &[], &[]);
        assert!(empty.is_empty());

        let plain = parse_message_blocks_arc("  Hello   world \n ", &[], &[]);
        assert_eq!(plain.len(), 1);
        assert_eq!(html_in::blocks_to_plain(&plain), "Hello world");

        let styled_html = "<p>  <a href=\"https://example.com\"><u><s><i><b>combo</b></i></s></u></a>  </p>";
        let styled_arc = parse_message_blocks_arc(styled_html, &[], &[]);
        let md = html_in::blocks_to_markdown(&styled_arc);
        assert_eq!(md, "[<u>~~***combo***~~</u>](https://example.com)");
    }

    #[test]
    fn test_extract_inline_image_attachments_skips_emojis_and_resolves_relative() {
        let emoji_html = r#"<p><emoji alt="🚀"><img itemtype="http://schema.skype.com/Emoji" src="https://statics.teams.cdn.office.net/emoji.png" alt="🚀"></emoji></p>"#;
        assert!(
            extract_inline_image_attachments(emoji_html, &[], "chat:19:abc", "m1", None)
                .is_empty()
        );

        let rel_html = r#"<p><img src="../hostedContents/1/$value"></p>"#;
        let chat_atts =
            extract_inline_image_attachments(rel_html, &[], "chat:19:abc", "m1", None);
        assert_eq!(chat_atts.len(), 1);
        assert_eq!(chat_atts[0].id, "hosted:1");
        assert_eq!(chat_atts[0].content_type, "hostedContent");
        assert_eq!(
            chat_atts[0].content_url.as_deref(),
            Some("/chats/19%3Aabc/messages/m1/hostedContents/1/$value")
        );
        assert_eq!(chat_atts[0].name.as_deref(), Some("inline-image-1.png"));

        let reply_atts = extract_inline_image_attachments(
            rel_html,
            &[],
            "chan:t1:c1",
            "reply-2",
            Some("root-1"),
        );
        assert_eq!(reply_atts.len(), 1);
        assert_eq!(
            reply_atts[0].content_url.as_deref(),
            Some("/teams/t1/channels/c1/messages/root-1/replies/reply-2/hostedContents/1/$value")
        );

        // Deduplicates when already present in `existing`
        assert!(
            extract_inline_image_attachments(rel_html, &chat_atts, "chat:19:abc", "m1", None)
                .is_empty()
        );
    }
}
