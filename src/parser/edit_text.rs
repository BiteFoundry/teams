//! Locate one text edit in the source, then verify it with the HTML parser.

use std::ops::Range;

use anyhow::{bail, Context, Result};
use markup5ever::data::{C1_REPLACEMENTS, NAMED_ENTITIES};
use scraper::{Html, Node};

use super::md_out::escape_html;

const NO_MATCH: &str = "Find text must match exactly once within a single editable text node.";

pub(crate) fn unique_match(text: &str, find: &str) -> Result<Range<usize>> {
    let first_char = find.chars().next().context("Find text is empty.")?;
    let start = text.find(find).context(NO_MATCH)?;
    if text[start + first_char.len_utf8()..].contains(find) {
        bail!("{NO_MATCH}");
    }
    Ok(start..start + find.len())
}

pub(crate) fn replace_html_text(html: &str, find: &str, replacement: &str) -> Result<String> {
    let mut expected = Html::parse_fragment(html);
    let mut text = String::new();
    let mut nodes = Vec::new();
    for node in expected.tree.root().descendants() {
        if let Node::Text(value) = node.value() {
            let protected = node
                .ancestors()
                .filter_map(|n| n.value().as_element())
                .any(|el| {
                    matches!(
                        el.name(),
                        "at" | "attachment"
                            | "emoji"
                            | "script"
                            | "style"
                            | "template"
                            | "title"
                            | "textarea"
                            | "noscript"
                            | "xmp"
                            | "iframe"
                            | "noembed"
                            | "noframes"
                            | "plaintext"
                    )
                });
            if protected {
                text.push('\0');
                continue;
            }
            let start = text.len();
            text.push_str(value);
            nodes.push((node.id(), start..text.len()));
        }
    }

    let match_span = unique_match(&text, find)?;
    let (id, node_span) = nodes
        .into_iter()
        .find(|(_, span)| span.start <= match_span.start && match_span.end <= span.end)
        .context(NO_MATCH)?;
    if find == replacement {
        return Ok(html.to_owned());
    }
    let mut new_text = text[node_span.clone()].to_owned();
    new_text.replace_range(
        match_span.start - node_span.start..match_span.end - node_span.start,
        replacement,
    );
    let mut edited_node = expected.tree.get_mut(id).context(NO_MATCH)?;
    if new_text.is_empty() {
        edited_node.detach();
    } else if let Node::Text(value) = edited_node.value() {
        value.text = new_text.into();
    }

    // Decode possible source spans, including references such as &amp;. A
    // candidate may start in markup or raw text. The DOM check below rejects
    // those spans and guarantees the edit changes only the selected node.
    let (source_text, source_offsets) = decode_source(html);
    let escaped = escape_html(replacement).replace('\r', "&#13;");
    let mut offset = 0;
    while let Some(index) = source_text[offset..].find(find) {
        let start = offset + index;
        let end = start + find.len();
        offset = start + source_text[start..].chars().next().unwrap().len_utf8();
        if let (Some(start), Some(end)) = (source_offsets[start], source_offsets[end]) {
            if html[start..end].match_indices('<').any(|(pos, _)| {
                html.as_bytes().get(start + pos + 1).is_some_and(|next| {
                    next.is_ascii_alphabetic() || matches!(*next, b'!' | b'?' | b'/')
                })
            }) {
                continue;
            }
            let mut edited = html.to_owned();
            edited.replace_range(start..end, &escaped);
            let actual = Html::parse_fragment(&edited);
            if actual
                .tree
                .root()
                .descendants()
                .map(|node| (node.ancestors().count(), node.value()))
                .eq(expected
                    .tree
                    .root()
                    .descendants()
                    .map(|node| (node.ancestors().count(), node.value())))
            {
                return Ok(edited);
            }
        }
    }
    bail!("{NO_MATCH}")
}

fn decode_source(html: &str) -> (String, Vec<Option<usize>>) {
    let mut text = String::with_capacity(html.len());
    let mut offsets = vec![Some(0)];
    let mut source = 0;
    while source < html.len() {
        let rest = &html[source..];
        let first = rest.chars().next().unwrap();
        if let Some((len, ch, extra)) = rest.strip_prefix('&').and_then(character_reference) {
            source += len + 1;
            text.push(ch);
            text.extend(extra);
        } else if first == '\r' {
            source += if rest.starts_with("\r\n") { 2 } else { 1 };
            text.push('\n');
        } else {
            source += first.len_utf8();
            text.push(first);
        }
        offsets.resize(text.len(), None);
        offsets.push(Some(source));
    }
    (text, offsets)
}

fn character_reference(input: &str) -> Option<(usize, char, Option<char>)> {
    if let Some(numeric) = input.strip_prefix('#') {
        let (digits, radix) = numeric
            .strip_prefix(['x', 'X'])
            .map_or((numeric, 10), |hex| (hex, 16));
        let mut value = 0u32;
        let len = digits
            .chars()
            .map_while(|c| c.to_digit(radix))
            .inspect(|digit| value = value.saturating_mul(radix).saturating_add(*digit))
            .count();
        if len == 0 {
            return None;
        }
        let semicolon = usize::from(digits[len..].starts_with(';'));
        let ch = match value {
            0 => '\u{fffd}',
            0x80..=0x9f => C1_REPLACEMENTS[value as usize - 0x80]
                .unwrap_or_else(|| char::from_u32(value).unwrap()),
            _ => char::from_u32(value).unwrap_or('\u{fffd}'),
        };
        return Some((input.len() - digits.len() + len + semicolon, ch, None));
    }

    // The parser's table includes prefixes, with a zero first code point
    // marking an incomplete name. Remember the longest complete reference.
    let mut matched = None;
    for (index, byte) in input.bytes().enumerate() {
        if !byte.is_ascii_alphanumeric() && byte != b';' {
            break;
        }
        let len = index + 1;
        let Some(&(first, extra)) = NAMED_ENTITIES.get(&input[..len]) else {
            break;
        };
        if let Some(ch) = char::from_u32(first).filter(|c| *c != '\0') {
            matched = Some((len, ch, char::from_u32(extra).filter(|c| *c != '\0')));
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_source_markup_and_escapes_replacement() {
        let original = "<DIV><a HREF='msg > link'>doc</a><!-- msg --><at id='0'>Alice</at>\
                        <strong>msg &amp; &#x1F680;</strong><img src='image'/></DIV>";
        assert_eq!(
            replace_html_text(original, "msg & 🚀", "한글 <b> & \r").unwrap(),
            original.replace("msg &amp; &#x1F680;", "한글 &lt;b&gt; &amp; &#13;")
        );
        assert_eq!(
            replace_html_text("<p><b>old</b></p>", "old", "").unwrap(),
            "<p><b></b></p>"
        );
        assert_eq!(
            replace_html_text("<code>STATE: READY</code>", "READY", "READY").unwrap(),
            "<code>STATE: READY</code>"
        );
    }

    #[test]
    fn rejects_non_unique_or_non_text_matches() {
        for (html, find) in [
            ("<b>aa</b><i>aa</i>", "aa"),
            ("<b>aaa</b>", "aa"),
            ("<p>one <b>two</b></p>", "one two"),
            ("<p>one two</p><p>one <b>two</b></p>", "one two"),
            ("<p class='old'><!--old--><at id='1'>old</at></p>", "old"),
            ("<script>old</script><style>old</style>", "old"),
            ("<p>unrelated</p>", "old"),
        ] {
            assert!(replace_html_text(html, find, "new").is_err(), "{html}");
        }
    }
}
