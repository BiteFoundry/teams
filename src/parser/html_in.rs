//! Teams HTML + attachment parser into `Vec<RichBlock>`, plain text, and GFM Markdown.

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use bitefoundry_html::pulldown_cmark::{
    CodeBlockKind, Event as MdEvent, Options as MdOptions, Parser as MdParser, Tag as MdTag,
    TagEnd as MdTagEnd,
};
use scraper::{ElementRef, Html, Node};

use crate::{
    models::{AttachmentInfo, MentionInfo, RichBlock, RichSpan, TextStyleFlags},
    parser::card_in::{is_card_content_type, parse_card_attachment},
};

const HTML_CACHE_MAX_ENTRIES: usize = 1024;
const HTML_CACHE_MAX_HTML_BYTES: usize = 256 * 1024;

type HtmlCacheKey = (u64, usize);

const LRU_NONE: usize = usize::MAX;

struct LruNode {
    key: HtmlCacheKey,
    value: Arc<[RichBlock]>,
    prev: usize,
    next: usize,
}

struct HtmlParseCache {
    index: HashMap<HtmlCacheKey, usize>,
    nodes: Vec<LruNode>,
    head: usize, // MRU
    tail: usize, // LRU
}

impl Default for HtmlParseCache {
    fn default() -> Self {
        Self {
            index: HashMap::new(),
            nodes: Vec::new(),
            head: LRU_NONE,
            tail: LRU_NONE,
        }
    }
}

impl HtmlParseCache {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.index.len()
    }

    fn detach(&mut self, idx: usize) {
        let prev = self.nodes[idx].prev;
        let next = self.nodes[idx].next;
        if prev != LRU_NONE {
            self.nodes[prev].next = next;
        } else {
            self.head = next;
        }
        if next != LRU_NONE {
            self.nodes[next].prev = prev;
        } else {
            self.tail = prev;
        }
        self.nodes[idx].prev = LRU_NONE;
        self.nodes[idx].next = LRU_NONE;
    }

    fn attach_front(&mut self, idx: usize) {
        self.nodes[idx].prev = LRU_NONE;
        self.nodes[idx].next = self.head;
        if self.head != LRU_NONE {
            self.nodes[self.head].prev = idx;
        } else {
            self.tail = idx;
        }
        self.head = idx;
    }

    fn promote_to_front(&mut self, idx: usize) {
        if self.head == idx {
            return;
        }
        self.detach(idx);
        self.attach_front(idx);
    }

    fn get(&mut self, key: &HtmlCacheKey) -> Option<Arc<[RichBlock]>> {
        let &idx = self.index.get(key)?;
        self.promote_to_front(idx);
        Some(Arc::clone(&self.nodes[idx].value))
    }

    fn insert(&mut self, key: HtmlCacheKey, value: Arc<[RichBlock]>) {
        if let Some(&idx) = self.index.get(&key) {
            self.nodes[idx].value = value;
            self.promote_to_front(idx);
            return;
        }
        if self.nodes.len() >= HTML_CACHE_MAX_ENTRIES {
            let victim = self.tail;
            if victim != LRU_NONE {
                let old_key = self.nodes[victim].key;
                self.index.remove(&old_key);
                self.detach(victim);
                self.nodes[victim].key = key;
                self.nodes[victim].value = value;
                self.attach_front(victim);
                self.index.insert(key, victim);
                return;
            }
        }
        let idx = self.nodes.len();
        self.nodes.push(LruNode {
            key,
            value,
            prev: LRU_NONE,
            next: LRU_NONE,
        });
        self.attach_front(idx);
        self.index.insert(key, idx);
    }
}

static HTML_PARSE_CACHE: OnceLock<Mutex<HtmlParseCache>> = OnceLock::new();

#[inline]
fn hash_str_field(h: &mut DefaultHasher, s: &str) {
    s.len().hash(h);
    h.write(s.as_bytes());
}

#[inline]
fn hash_opt_str_field(h: &mut DefaultHasher, opt: Option<&str>) {
    match opt {
        Some(s) => {
            1u8.hash(h);
            hash_str_field(h, s);
        }
        None => 0u8.hash(h),
    }
}

fn compute_parse_fingerprint(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> HtmlCacheKey {
    let mut h = DefaultHasher::new();

    0x4854_4d4c_u32.hash(&mut h);
    hash_str_field(&mut h, html);

    attachments.len().hash(&mut h);
    for att in attachments {
        hash_str_field(&mut h, &att.id);
        hash_str_field(&mut h, &att.content_type);
        hash_opt_str_field(&mut h, att.name.as_deref());
        hash_opt_str_field(&mut h, att.content_url.as_deref());
        hash_opt_str_field(&mut h, att.content_json.as_deref());
        hash_opt_str_field(&mut h, att.thumbnail_url.as_deref());
    }

    mentions.len().hash(&mut h);
    for m in mentions {
        m.id.hash(&mut h);
        hash_str_field(&mut h, &m.user_id);
        hash_str_field(&mut h, &m.display_name);
    }

    (h.finish(), html.len())
}

fn decode_named_html_entity(name: &str) -> Option<char> {
    match name {
        "amp" | "AMP" => Some('&'),
        "lt" | "LT" => Some('<'),
        "gt" | "GT" => Some('>'),
        "quot" | "QUOT" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some('\u{00A0}'),
        "ndash" => Some('\u{2013}'),
        "mdash" => Some('\u{2014}'),
        "hellip" => Some('\u{2026}'),
        "lsquo" => Some('\u{2018}'),
        "rsquo" => Some('\u{2019}'),
        "ldquo" => Some('\u{201C}'),
        "rdquo" => Some('\u{201D}'),
        "bull" => Some('\u{2022}'),
        "middot" => Some('\u{00B7}'),
        "copy" | "COPY" => Some('\u{00A9}'),
        "reg" | "REG" => Some('\u{00AE}'),
        "trade" => Some('\u{2122}'),
        "times" => Some('\u{00D7}'),
        "divide" => Some('\u{00F7}'),
        "plusmn" => Some('\u{00B1}'),
        "larr" => Some('\u{2190}'),
        "rarr" => Some('\u{2192}'),
        "uarr" => Some('\u{2191}'),
        "darr" => Some('\u{2193}'),
        "euro" => Some('\u{20AC}'),
        "pound" => Some('\u{00A3}'),
        "yen" => Some('\u{00A5}'),
        _ => None,
    }
}

fn decode_numeric_html_entity(body: &str) -> Option<char> {
    let cp = if let Some(hex) = body.strip_prefix('x').or_else(|| body.strip_prefix('X')) {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(hex, 16).unwrap_or(0x11_0000)
    } else {
        if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        body.parse::<u32>().unwrap_or(0x11_0000)
    };
    if cp == 0 {
        return Some('\u{FFFD}');
    }
    Some(char::from_u32(cp).unwrap_or('\u{FFFD}'))
}

fn decode_plain_entities_and_newlines(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        match bytes[i] {
            b'\r' => {
                if i + 1 < len && bytes[i + 1] == b'\n' {
                    i += 2;
                } else {
                    i += 1;
                }
                out.push('\n');
            }
            b'&' => {
                let mut decoded_char = None;
                let max_end = (i + 16).min(len);
                if let Some(rel_semi) = bytes[i + 1..max_end].iter().position(|&b| b == b';') {
                    let entity = &input[i + 1..i + 1 + rel_semi];
                    decoded_char = if let Some(num) = entity.strip_prefix('#') {
                        decode_numeric_html_entity(num)
                    } else if entity.bytes().all(|b| b.is_ascii_alphanumeric()) {
                        decode_named_html_entity(entity)
                    } else {
                        None
                    };
                    if let Some(ch) = decoded_char {
                        out.push(ch);
                        i = i + 1 + rel_semi + 1;
                    }
                }
                if decoded_char.is_none() {
                    out.push('&');
                    i += 1;
                }
            }
            _ => {
                let start = i;
                i += 1;
                while i < len && bytes[i] != b'&' && bytes[i] != b'\r' {
                    i += 1;
                }
                out.push_str(&input[start..i]);
            }
        }
    }
    out
}

fn parse_plain_with_entities_preserving_newlines(input: &str) -> Vec<RichBlock> {
    let normalized = decode_plain_entities_and_newlines(input);
    let trimmed = normalized.trim_matches(|c| c == ' ' || c == '\t' || c == '\n');
    if trimmed.is_empty() {
        Vec::new()
    } else {
        vec![RichBlock::Paragraph(vec![RichSpan::plain(trimmed)])]
    }
}

fn try_fast_path_plain_or_empty(
    html: &str,
    attachments: &[AttachmentInfo],
) -> Option<Arc<[RichBlock]>> {
    if !attachments.is_empty() {
        return None;
    }
    if html.trim().is_empty() {
        return Some(Arc::from([]));
    }
    if !html.contains('<') && !html.contains('\0') {
        if !html.contains('&') {
            return Some(Arc::from(parse_plain_markdown_blocks(html)));
        }
        return Some(Arc::from(parse_plain_with_entities_preserving_newlines(html)));
    }
    None
}

/// Parses an HTML-free plain-text / Markdown message into `Vec<RichBlock>` via
/// `bitefoundry_html::pulldown_cmark`, preserving explicit `\n` line breaks and Markdown formatting.
pub fn parse_plain_markdown_blocks(input: &str) -> Vec<RichBlock> {
    if input.trim().is_empty() {
        return Vec::new();
    }

    let mut opts = MdOptions::empty();
    opts.insert(MdOptions::ENABLE_STRIKETHROUGH);
    opts.insert(MdOptions::ENABLE_TASKLISTS);
    opts.insert(MdOptions::ENABLE_TABLES);

    enum ActiveBlock {
        Paragraph(Vec<RichSpan>),
        Heading { level: u8, spans: Vec<RichSpan> },
        Quote(Vec<RichSpan>),
        Code { lang: Option<String>, code: String },
        UnorderedList { items: Vec<Vec<RichSpan>>, current: Option<Vec<RichSpan>> },
        OrderedList { next_num: u64, spans: Vec<RichSpan>, current: Option<Vec<RichSpan>>, emitted: usize },
        Table { spans: Vec<RichSpan>, rows: usize, in_head: bool, cell_idx: usize },
    }

    let push_span = |buf: &mut Vec<RichSpan>, text: &str, style: &TextStyleFlags| {
        if text.is_empty() {
            return;
        }
        if let Some(last) = buf.last_mut() {
            if &last.style == style {
                last.text.push_str(text);
                return;
            }
        }
        buf.push(RichSpan {
            text: text.to_string(),
            style: style.clone(),
        });
    };

    let default_style = TextStyleFlags::default();
    let mut style_stack: Vec<TextStyleFlags> = vec![default_style.clone()];
    let mut blocks: Vec<RichBlock> = Vec::new();
    let mut active: Option<ActiveBlock> = None;
    let mut nested_lists: Vec<Option<u64>> = Vec::new();
    let mut quote_depth: usize = 0;

    let flush_active = |active: &mut Option<ActiveBlock>, blocks: &mut Vec<RichBlock>| {
        let Some(blk) = active.take() else {
            return;
        };
        match blk {
            ActiveBlock::Paragraph(spans) => {
                if !spans_are_blank(&spans) {
                    blocks.push(RichBlock::Paragraph(trim_spans(spans)));
                }
            }
            ActiveBlock::Heading { level, spans } => {
                if !spans_are_blank(&spans) {
                    blocks.push(RichBlock::Heading {
                        level,
                        spans: trim_spans(spans),
                    });
                }
            }
            ActiveBlock::Quote(spans) => {
                if !spans_are_blank(&spans) {
                    blocks.push(RichBlock::Quote(trim_spans(spans)));
                }
            }
            ActiveBlock::Code { lang, code } => {
                let trimmed = code.trim_matches(|c| c == '\r' || c == '\n');
                if !trimmed.is_empty() {
                    blocks.push(RichBlock::CodeBlock {
                        lang,
                        code: trimmed.to_string(),
                    });
                }
            }
            ActiveBlock::UnorderedList { mut items, current } => {
                if let Some(cur) = current {
                    if !spans_are_blank(&cur) {
                        items.push(trim_spans(cur));
                    }
                }
                if !items.is_empty() {
                    blocks.push(RichBlock::BulletList(items));
                }
            }
            ActiveBlock::OrderedList {
                mut next_num,
                mut spans,
                current,
                mut emitted,
            } => {
                if let Some(cur) = current {
                    if !spans_are_blank(&cur) {
                        if emitted > 0 {
                            spans.push(RichSpan::plain("\n"));
                        }
                        spans.push(RichSpan::plain(format!("{next_num}. ")));
                        spans.extend(trim_spans(cur));
                        next_num += 1;
                        emitted += 1;
                    }
                }
                let _ = (next_num, emitted);
                if !spans_are_blank(&spans) {
                    blocks.push(RichBlock::Paragraph(spans));
                }
            }
            ActiveBlock::Table { spans, .. } => {
                if !spans_are_blank(&spans) {
                    blocks.push(RichBlock::Paragraph(trim_spans(spans)));
                }
            }
        }
    };

    for event in MdParser::new_ext(input, opts) {
        match event {
            MdEvent::Start(tag) => match tag {
                MdTag::Paragraph => match active.as_mut() {
                    Some(ActiveBlock::Quote(spans)) => {
                        if !spans.is_empty() && !spans.last().is_some_and(|s| s.text.ends_with('\n')) {
                            push_span(spans, "\n", &default_style);
                        }
                    }
                    Some(ActiveBlock::UnorderedList { current: Some(cur), .. })
                    | Some(ActiveBlock::OrderedList { current: Some(cur), .. }) => {
                        if !cur.is_empty() && !cur.last().is_some_and(|s| s.text.ends_with('\n')) {
                            push_span(cur, "\n", &default_style);
                        }
                    }
                    _ => {
                        flush_active(&mut active, &mut blocks);
                        active = Some(ActiveBlock::Paragraph(Vec::new()));
                    }
                },
                MdTag::Heading { level, .. } => {
                    flush_active(&mut active, &mut blocks);
                    active = Some(ActiveBlock::Heading {
                        level: level as u8,
                        spans: Vec::new(),
                    });
                }
                MdTag::BlockQuote(_) => {
                    quote_depth += 1;
                    if quote_depth == 1 {
                        flush_active(&mut active, &mut blocks);
                        active = Some(ActiveBlock::Quote(Vec::new()));
                    }
                }
                MdTag::CodeBlock(kind) => {
                    flush_active(&mut active, &mut blocks);
                    let lang = match kind {
                        CodeBlockKind::Fenced(info) => info
                            .split_whitespace()
                            .next()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string),
                        CodeBlockKind::Indented => None,
                    };
                    active = Some(ActiveBlock::Code {
                        lang,
                        code: String::new(),
                    });
                }
                MdTag::List(start) => match active.as_mut() {
                    Some(ActiveBlock::UnorderedList { .. })
                    | Some(ActiveBlock::OrderedList { .. })
                    | Some(ActiveBlock::Quote(_)) => {
                        nested_lists.push(start);
                    }
                    _ => {
                        flush_active(&mut active, &mut blocks);
                        active = Some(match start {
                            Some(n) => ActiveBlock::OrderedList {
                                next_num: n,
                                spans: Vec::new(),
                                current: None,
                                emitted: 0,
                            },
                            None => ActiveBlock::UnorderedList {
                                items: Vec::new(),
                                current: None,
                            },
                        });
                    }
                },
                MdTag::Item => {
                    if !nested_lists.is_empty() {
                        let target = match active.as_mut() {
                            Some(ActiveBlock::UnorderedList { current: Some(cur), .. })
                            | Some(ActiveBlock::OrderedList { current: Some(cur), .. })
                            | Some(ActiveBlock::Quote(cur)) => Some(cur),
                            _ => None,
                        };
                        if let Some(cur) = target {
                            if !cur.is_empty() && !cur.last().is_some_and(|s| s.text.ends_with('\n')) {
                                push_span(cur, "\n", &default_style);
                            }
                            let indent = "  ".repeat(nested_lists.len());
                            push_span(cur, &indent, &default_style);
                            match nested_lists.last_mut() {
                                Some(Some(n)) => {
                                    let marker = format!("{n}. ");
                                    *n += 1;
                                    push_span(cur, &marker, &default_style);
                                }
                                _ => push_span(cur, "• ", &default_style),
                            }
                        }
                    } else {
                        match active.as_mut() {
                            Some(ActiveBlock::UnorderedList { current, .. }) => {
                                *current = Some(Vec::new());
                            }
                            Some(ActiveBlock::OrderedList { current, .. }) => {
                                *current = Some(Vec::new());
                            }
                            _ => {}
                        }
                    }
                }
                MdTag::Table(_) => {
                    flush_active(&mut active, &mut blocks);
                    active = Some(ActiveBlock::Table {
                        spans: Vec::new(),
                        rows: 0,
                        in_head: false,
                        cell_idx: 0,
                    });
                }
                MdTag::TableHead => {
                    if let Some(ActiveBlock::Table {
                        spans,
                        rows,
                        in_head,
                        cell_idx,
                    }) = active.as_mut()
                    {
                        if *rows > 0 {
                            push_span(spans, "\n", &default_style);
                        }
                        *in_head = true;
                        *cell_idx = 0;
                    }
                }
                MdTag::TableRow => {
                    if let Some(ActiveBlock::Table {
                        spans,
                        rows,
                        in_head,
                        cell_idx,
                    }) = active.as_mut()
                    {
                        if *rows > 0 {
                            push_span(spans, "\n", &default_style);
                        }
                        *in_head = false;
                        *cell_idx = 0;
                    }
                }
                MdTag::TableCell => {
                    if let Some(ActiveBlock::Table {
                        spans,
                        in_head,
                        cell_idx,
                        ..
                    }) = active.as_mut()
                    {
                        if *cell_idx > 0 {
                            push_span(spans, " │ ", &default_style);
                        }
                        *cell_idx += 1;
                        if *in_head {
                            let mut next = style_stack.last().cloned().unwrap_or_default();
                            next.bold = true;
                            style_stack.push(next);
                        }
                    }
                }
                MdTag::Strong => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.bold = true;
                    style_stack.push(next);
                }
                MdTag::Emphasis => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.italic = true;
                    style_stack.push(next);
                }
                MdTag::Strikethrough => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    next.strike = true;
                    style_stack.push(next);
                }
                MdTag::Link { dest_url, .. } => {
                    let mut next = style_stack.last().cloned().unwrap_or_default();
                    let url = dest_url.trim();
                    if !url.is_empty() {
                        next.link_url = Some(url.to_string());
                    }
                    style_stack.push(next);
                }
                _ => {}
            },
            MdEvent::End(tag_end) => match tag_end {
                MdTagEnd::Paragraph => {
                    if matches!(active, Some(ActiveBlock::Paragraph(_))) {
                        flush_active(&mut active, &mut blocks);
                    }
                }
                MdTagEnd::Heading(_) => {
                    if matches!(active, Some(ActiveBlock::Heading { .. })) {
                        flush_active(&mut active, &mut blocks);
                    }
                }
                MdTagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                    if quote_depth == 0 && matches!(active, Some(ActiveBlock::Quote(_))) {
                        flush_active(&mut active, &mut blocks);
                    }
                }
                MdTagEnd::CodeBlock => {
                    if matches!(active, Some(ActiveBlock::Code { .. })) {
                        flush_active(&mut active, &mut blocks);
                    }
                }
                MdTagEnd::List(_) => {
                    if !nested_lists.is_empty() {
                        nested_lists.pop();
                    } else if matches!(
                        active,
                        Some(ActiveBlock::UnorderedList { .. } | ActiveBlock::OrderedList { .. })
                    ) {
                        flush_active(&mut active, &mut blocks);
                    }
                }
                MdTagEnd::Item => {
                    if nested_lists.is_empty() {
                        match active.as_mut() {
                            Some(ActiveBlock::UnorderedList { items, current }) => {
                                if let Some(cur) = current.take() {
                                    if !spans_are_blank(&cur) {
                                        items.push(trim_spans(cur));
                                    }
                                }
                            }
                            Some(ActiveBlock::OrderedList {
                                next_num,
                                spans,
                                current,
                                emitted,
                            }) => {
                                if let Some(cur) = current.take() {
                                    if !spans_are_blank(&cur) {
                                        if *emitted > 0 {
                                            spans.push(RichSpan::plain("\n"));
                                        }
                                        spans.push(RichSpan::plain(format!("{next_num}. ")));
                                        spans.extend(trim_spans(cur));
                                        *next_num += 1;
                                        *emitted += 1;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                MdTagEnd::TableHead => {
                    if let Some(ActiveBlock::Table { rows, in_head, .. }) = active.as_mut() {
                        *in_head = false;
                        *rows += 1;
                    }
                }
                MdTagEnd::TableRow => {
                    if let Some(ActiveBlock::Table { rows, .. }) = active.as_mut() {
                        *rows += 1;
                    }
                }
                MdTagEnd::TableCell => {
                    if matches!(active, Some(ActiveBlock::Table { in_head: true, .. }))
                        && style_stack.len() > 1
                    {
                        style_stack.pop();
                    }
                }
                MdTagEnd::Table => {
                    if matches!(active, Some(ActiveBlock::Table { .. })) {
                        flush_active(&mut active, &mut blocks);
                    }
                }
                MdTagEnd::Strong
                | MdTagEnd::Emphasis
                | MdTagEnd::Strikethrough
                | MdTagEnd::Link
                    if style_stack.len() > 1 =>
                {
                    style_stack.pop();
                }
                _ => {}
            },
            MdEvent::Text(text) => {
                if let Some(ActiveBlock::Code { code, .. }) = active.as_mut() {
                    code.push_str(&text);
                } else {
                    let style = style_stack.last().unwrap_or(&default_style);
                    if active.is_none() {
                        active = Some(ActiveBlock::Paragraph(Vec::new()));
                    }
                    let target = match active.as_mut() {
                        Some(ActiveBlock::Paragraph(s))
                        | Some(ActiveBlock::Heading { spans: s, .. })
                        | Some(ActiveBlock::Quote(s))
                        | Some(ActiveBlock::Table { spans: s, .. }) => Some(s),
                        Some(ActiveBlock::UnorderedList { current: Some(s), .. })
                        | Some(ActiveBlock::OrderedList { current: Some(s), .. }) => Some(s),
                        _ => None,
                    };
                    if let Some(buf) = target {
                        let normalized = collapse_inline_spaces(&text);
                        push_span(buf, &normalized, style);
                    }
                }
            }
            MdEvent::Code(code_str) => {
                let mut style = style_stack.last().cloned().unwrap_or_default();
                style.code = true;
                if active.is_none() {
                    active = Some(ActiveBlock::Paragraph(Vec::new()));
                }
                let target = match active.as_mut() {
                    Some(ActiveBlock::Paragraph(s))
                    | Some(ActiveBlock::Heading { spans: s, .. })
                    | Some(ActiveBlock::Quote(s))
                    | Some(ActiveBlock::Table { spans: s, .. }) => Some(s),
                    Some(ActiveBlock::UnorderedList { current: Some(s), .. })
                    | Some(ActiveBlock::OrderedList { current: Some(s), .. }) => Some(s),
                    _ => None,
                };
                if let Some(buf) = target {
                    push_span(buf, &code_str, &style);
                }
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                let target = match active.as_mut() {
                    Some(ActiveBlock::Paragraph(s))
                    | Some(ActiveBlock::Heading { spans: s, .. })
                    | Some(ActiveBlock::Quote(s)) => Some(s),
                    Some(ActiveBlock::UnorderedList { current: Some(s), .. })
                    | Some(ActiveBlock::OrderedList { current: Some(s), .. }) => Some(s),
                    _ => None,
                };
                if let Some(buf) = target {
                    push_span(buf, "\n", &default_style);
                }
            }
            MdEvent::TaskListMarker(checked) => {
                let style = style_stack.last().unwrap_or(&default_style);
                let target = match active.as_mut() {
                    Some(ActiveBlock::UnorderedList { current: Some(s), .. })
                    | Some(ActiveBlock::OrderedList { current: Some(s), .. })
                    | Some(ActiveBlock::Quote(s))
                    | Some(ActiveBlock::Paragraph(s)) => Some(s),
                    _ => None,
                };
                if let Some(buf) = target {
                    push_span(buf, if checked { "[x] " } else { "[ ] " }, style);
                }
            }
            MdEvent::Rule => {
                flush_active(&mut active, &mut blocks);
                blocks.push(RichBlock::Paragraph(vec![RichSpan::plain(
                    "────────────────────",
                )]));
            }
            _ => {}
        }
    }

    flush_active(&mut active, &mut blocks);
    blocks
}

pub fn parse_message_blocks_arc(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> Arc<[RichBlock]> {
    if let Some(fast) = try_fast_path_plain_or_empty(html, attachments) {
        return fast;
    }
    if html.len() > HTML_CACHE_MAX_HTML_BYTES {
        return Arc::from(parse_message_blocks_uncached(html, attachments, mentions));
    }

    let key = compute_parse_fingerprint(html, attachments, mentions);
    let cache = HTML_PARSE_CACHE.get_or_init(|| Mutex::new(HtmlParseCache::default()));
    {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = guard.get(&key) {
            return cached;
        }
    }

    let parsed: Arc<[RichBlock]> =
        Arc::from(parse_message_blocks_uncached(html, attachments, mentions));
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(key, Arc::clone(&parsed));
    parsed
}

pub fn parse_message_blocks(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> Vec<RichBlock> {
    parse_message_blocks_arc(html, attachments, mentions).to_vec()
}

pub(crate) fn parse_message_blocks_no_cache(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> Arc<[RichBlock]> {
    if let Some(fast) = try_fast_path_plain_or_empty(html, attachments) {
        return fast;
    }
    Arc::from(parse_message_blocks_uncached(html, attachments, mentions))
}

fn parse_message_blocks_uncached(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> Vec<RichBlock> {
    let mut ctx = ParserContext {
        attachments,
        mentions,
        seen_attachments: HashSet::new(),
        seen_inline_srcs: HashSet::new(),
        seen_hosted_ids: HashSet::new(),
        blocks: Vec::new(),
        current_spans: Vec::new(),
        next_image_index: 0,
    };

    if !html.trim().is_empty() {
        if !html.contains('<') && !html.contains('\0') {
            if !html.contains('&') {
                ctx.blocks.extend(parse_plain_markdown_blocks(html));
            } else {
                ctx.blocks
                    .extend(parse_plain_with_entities_preserving_newlines(html));
            }
        } else {
            let fragment = Html::parse_fragment(html);
            ctx.walk_children(fragment.root_element(), &TextStyleFlags::default());
            ctx.flush_paragraph();
        }
    }

    for att in attachments {
        if !ctx.seen_attachments.contains(&att.id)
            && !ctx.is_seen_inline_attachment(att)
            && !att.is_message_reference()
        {
            ctx.push_attachment_block(att);
        }
    }

    ctx.blocks
}

pub fn blocks_to_plain(blocks: &[RichBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            RichBlock::Paragraph(spans) => {
                let s = spans_to_plain(spans);
                if !s.trim().is_empty() {
                    parts.push(s.trim().to_string());
                }
            }
            RichBlock::Heading { spans, .. } => {
                let s = spans_to_plain(spans);
                if !s.trim().is_empty() {
                    parts.push(s.trim().to_string());
                }
            }
            RichBlock::Quote(spans) => {
                let s = spans_to_plain(spans);
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    let quoted = trimmed
                        .lines()
                        .map(|l| format!("> {}", l.trim()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    parts.push(quoted);
                }
            }
            RichBlock::CodeBlock { code, .. } => {
                let trimmed = code.trim_end();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
            RichBlock::BulletList(items) => {
                let mut lines = Vec::new();
                for item in items {
                    let s = spans_to_plain(item);
                    if !s.trim().is_empty() {
                        lines.push(format!("- {}", s.trim()));
                    }
                }
                if !lines.is_empty() {
                    parts.push(lines.join("\n"));
                }
            }
            RichBlock::Card {
                title,
                subtitle,
                facts,
                body_lines,
                actions,
            } => {
                let card_plain = |raw: &str| -> String {
                    let spans = crate::parser::card_in::parse_card_markdown_spans(raw);
                    if spans.is_empty() {
                        raw.to_string()
                    } else {
                        spans_to_plain(&spans)
                    }
                };
                let mut lines = Vec::new();
                if let Some(t) = title.as_deref().filter(|s| !s.is_empty()) {
                    lines.push(card_plain(t));
                }
                if let Some(sub) = subtitle.as_deref().filter(|s| !s.is_empty()) {
                    lines.push(card_plain(sub));
                }
                for bl in body_lines {
                    if !bl.is_empty() {
                        lines.push(card_plain(bl));
                    }
                }
                for (k, v) in facts {
                    let kp = card_plain(k);
                    let vp = card_plain(v);
                    if kp.is_empty() {
                        lines.push(vp);
                    } else {
                        lines.push(format!("{kp}: {vp}"));
                    }
                }
                for (label, target) in actions {
                    if target.is_empty() {
                        lines.push(format!("[{label}]"));
                    } else {
                        lines.push(format!("[{label}] ({target})"));
                    }
                }
                if !lines.is_empty() {
                    parts.push(lines.join("\n"));
                }
            }
            RichBlock::AttachmentChip { id, name } => {
                let label = name.as_deref().unwrap_or(id.as_str());
                parts.push(format!("[Attachment: {label}]"));
            }
            RichBlock::InlineImageChip { index, .. } => {
                parts.push(format!("[Image #{}]", index + 1));
            }
        }
    }
    parts.join("\n")
}

pub fn blocks_to_markdown(blocks: &[RichBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            RichBlock::Paragraph(spans) => {
                let s = spans_to_markdown(spans);
                if !s.trim().is_empty() {
                    parts.push(s.trim().to_string());
                }
            }
            RichBlock::Heading { level, spans } => {
                let s = spans_to_markdown(spans);
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    let hashes = "#".repeat((*level).clamp(1, 6) as usize);
                    parts.push(format!("{hashes} {trimmed}"));
                }
            }
            RichBlock::Quote(spans) => {
                let s = spans_to_markdown(spans);
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    let quoted = trimmed
                        .lines()
                        .map(|l| format!("> {}", l.trim_end()))
                        .collect::<Vec<_>>()
                        .join("\n");
                    parts.push(quoted);
                }
            }
            RichBlock::CodeBlock { lang, code } => {
                let lang_str = lang.as_deref().unwrap_or("");
                parts.push(format!("```{lang_str}\n{}\n```", code.trim_end_matches('\n')));
            }
            RichBlock::BulletList(items) => {
                let mut lines = Vec::new();
                for item in items {
                    let s = spans_to_markdown(item);
                    if !s.trim().is_empty() {
                        lines.push(format!("- {}", s.trim()));
                    }
                }
                if !lines.is_empty() {
                    parts.push(lines.join("\n"));
                }
            }
            RichBlock::Card {
                title,
                subtitle,
                facts,
                body_lines,
                actions,
            } => {
                let mut lines = Vec::new();
                if let Some(t) = title.as_deref().filter(|s| !s.is_empty()) {
                    lines.push(format!("> **{t}**"));
                }
                if let Some(sub) = subtitle.as_deref().filter(|s| !s.is_empty()) {
                    lines.push(format!("> *{sub}*"));
                }
                for bl in body_lines {
                    if !bl.is_empty() {
                        lines.push(format!("> {bl}"));
                    }
                }
                for (k, v) in facts {
                    lines.push(format!("> - **{k}**: {v}"));
                }
                if !actions.is_empty() {
                    let btns: Vec<String> = actions
                        .iter()
                        .map(|(label, url)| {
                            if url.starts_with("http://") || url.starts_with("https://") {
                                format!("[{label}]({url})")
                            } else {
                                format!("[{label}]")
                            }
                        })
                        .collect();
                    lines.push(format!("> {}", btns.join(" · ")));
                }
                if !lines.is_empty() {
                    parts.push(lines.join("\n"));
                }
            }
            RichBlock::AttachmentChip { id, name } => {
                let label = name.as_deref().unwrap_or(id.as_str());
                parts.push(format!("[Attachment: {label}]"));
            }
            RichBlock::InlineImageChip { src, index } => {
                parts.push(format!("![image-{}]({src})", index + 1));
            }
        }
    }
    parts.join("\n\n")
}

struct ParserContext<'a> {
    attachments: &'a [AttachmentInfo],
    mentions: &'a [MentionInfo],
    seen_attachments: HashSet<String>,
    seen_inline_srcs: HashSet<String>,
    seen_hosted_ids: HashSet<String>,
    blocks: Vec<RichBlock>,
    current_spans: Vec<RichSpan>,
    next_image_index: usize,
}

impl<'a> ParserContext<'a> {
    fn walk_children(&mut self, parent: ElementRef<'_>, style: &TextStyleFlags) {
        for child in parent.children() {
            match child.value() {
                Node::Text(text) => {
                    self.push_inline_text(text.text.as_ref(), style);
                }
                Node::Element(_) => {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        self.handle_element(child_el, style);
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_element(&mut self, el_ref: ElementRef<'_>, style: &TextStyleFlags) {
        let el = el_ref.value();
        let tag = el.name();
        if is_ignored_noise_tag(tag) || has_display_none(el) {
            return;
        }
        match tag {
            "br" => {
                self.push_raw_span("\n", &TextStyleFlags::default());
            }
            "b" | "strong" => {
                let mut next = style.clone();
                next.bold = true;
                self.walk_children(el_ref, &next);
            }
            "i" | "em" => {
                let mut next = style.clone();
                next.italic = true;
                self.walk_children(el_ref, &next);
            }
            "s" | "strike" | "del" => {
                let mut next = style.clone();
                next.strike = true;
                self.walk_children(el_ref, &next);
            }
            "u" => {
                let mut next = style.clone();
                next.underline = true;
                self.walk_children(el_ref, &next);
            }
            "code" => {
                let mut next = style.clone();
                next.code = true;
                let raw = collect_raw_text(el_ref);
                if !raw.is_empty() {
                    self.push_raw_span(&raw, &next);
                }
            }
            "a" => {
                let mut next = style.clone();
                if let Some(href) = el.attr("href").map(str::trim).filter(|s| !s.is_empty()) {
                    next.link_url = Some(href.to_string());
                }
                self.walk_children(el_ref, &next);
            }
            "at" => {
                let mut next = style.clone();
                next.mention = true;
                let inner = collect_raw_text(el_ref);
                let trimmed = inner.trim();
                let name = if !trimmed.is_empty() {
                    trimmed.to_string()
                } else if let Some(id_val) = el.attr("id").and_then(|v| v.parse::<i32>().ok()) {
                    self.mentions
                        .iter()
                        .find(|m| m.id == id_val)
                        .map(|m| m.display_name.clone())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                if !name.is_empty() {
                    let display = if name.starts_with('@') {
                        name
                    } else {
                        format!("@{name}")
                    };
                    self.push_raw_span(&display, &next);
                }
            }
            "p" | "div" => {
                self.flush_paragraph();
                self.walk_children(el_ref, style);
                self.flush_paragraph();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush_paragraph();
                let level = tag[1..].parse::<u8>().unwrap_or(1);
                let spans = self.collect_inline_spans(el_ref, style);
                if !spans_are_blank(&spans) {
                    self.blocks.push(RichBlock::Heading {
                        level,
                        spans: trim_spans(spans),
                    });
                }
            }
            "blockquote" => {
                self.flush_paragraph();
                let spans = self.collect_quote_spans(el_ref, style);
                if !spans_are_blank(&spans) {
                    self.blocks.push(RichBlock::Quote(trim_spans(spans)));
                }
            }
            "pre" => {
                self.flush_paragraph();
                let lang = detect_code_lang(el_ref);
                let code = collect_raw_text(el_ref);
                let trimmed = code.trim_matches(|c| c == '\r' || c == '\n');
                if !trimmed.is_empty() {
                    self.blocks.push(RichBlock::CodeBlock {
                        lang,
                        code: trimmed.to_string(),
                    });
                }
            }
            "ul" => {
                self.flush_paragraph();
                let mut items = Vec::new();
                for child in el_ref.children() {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        if child_el.value().name() == "li" {
                            let spans = self.collect_inline_spans(child_el, style);
                            if !spans_are_blank(&spans) {
                                items.push(trim_spans(spans));
                            }
                        }
                    }
                }
                if !items.is_empty() {
                    self.blocks.push(RichBlock::BulletList(items));
                }
            }
            "ol" => {
                self.flush_paragraph();
                let start = el
                    .attr("start")
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(1);
                let mut ordered_spans: Vec<RichSpan> = Vec::new();
                let mut num = start;
                let mut emitted = 0usize;
                for child in el_ref.children() {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        if child_el.value().name() == "li" {
                            let spans = self.collect_inline_spans(child_el, style);
                            if !spans_are_blank(&spans) {
                                if emitted > 0 {
                                    ordered_spans.push(RichSpan::plain("\n"));
                                }
                                ordered_spans.push(RichSpan::plain(format!("{num}. ")));
                                ordered_spans.extend(trim_spans(spans));
                                num += 1;
                                emitted += 1;
                            }
                        }
                    }
                }
                if !ordered_spans.is_empty() {
                    self.blocks.push(RichBlock::Paragraph(ordered_spans));
                }
            }
            "hr" => {
                self.flush_paragraph();
                self.blocks
                    .push(RichBlock::Paragraph(vec![RichSpan::plain(
                        "────────────────────",
                    )]));
            }
            "attachment" => {
                self.flush_paragraph();
                if let Some(id) = el.attr("id").map(str::trim).filter(|s| !s.is_empty()) {
                    self.seen_attachments.insert(id.to_string());
                    if let Some(att) = self.attachments.iter().find(|a| a.id == id) {
                        if !att.is_message_reference() {
                            self.push_attachment_block(att);
                        }
                    } else {
                        self.blocks.push(RichBlock::AttachmentChip {
                            id: id.to_string(),
                            name: None,
                        });
                    }
                }
            }
            "emoji" => {
                let emoji_text = non_empty_attr(el, "alt")
                    .or_else(|| non_empty_attr(el, "title"))
                    .map(str::to_string)
                    .or_else(|| {
                        el_ref.descendants().find_map(|desc| {
                            let desc_el = ElementRef::wrap(desc)?;
                            if desc_el.value().name() == "img" {
                                non_empty_attr(desc_el.value(), "alt")
                                    .or_else(|| non_empty_attr(desc_el.value(), "title"))
                                    .map(str::to_string)
                            } else {
                                None
                            }
                        })
                    });
                if let Some(alt) = emoji_text {
                    self.push_raw_span(&alt, style);
                } else {
                    self.walk_children(el_ref, style);
                }
            }
            "img" => {
                if is_emoji_sprite_img(el) {
                    if let Some(alt) =
                        non_empty_attr(el, "alt").or_else(|| non_empty_attr(el, "title"))
                    {
                        self.push_raw_span(alt, style);
                    }
                    return;
                }
                if let Some(src) = el.attr("src").map(str::trim).filter(|s| !s.is_empty()) {
                    self.flush_paragraph();
                    self.seen_inline_srcs.insert(src.to_string());
                    if let Some(hid) = extract_hosted_content_id(src) {
                        self.seen_hosted_ids.insert(hid.to_string());
                    }
                    let index = self.next_image_index;
                    self.next_image_index += 1;
                    self.blocks.push(RichBlock::InlineImageChip {
                        src: src.to_string(),
                        index,
                    });
                }
            }
            "table" => {
                self.flush_paragraph();
                if should_linearize_table(el_ref) {
                    self.linearize_table(el_ref, style);
                } else {
                    self.render_data_table(el_ref, style);
                }
            }
            _ => {
                self.walk_children(el_ref, style);
            }
        }
    }

    fn linearize_table(&mut self, table_el: ElementRef<'_>, style: &TextStyleFlags) {
        for child in table_el.children() {
            match child.value() {
                Node::Text(text) => {
                    self.push_inline_text(text.text.as_ref(), style);
                }
                Node::Element(el) => {
                    if is_ignored_noise_tag(el.name()) || has_display_none(el) {
                        continue;
                    }
                    let Some(child_el) = ElementRef::wrap(child) else {
                        continue;
                    };
                    match el.name() {
                        "thead" | "tbody" | "tfoot" => {
                            for row_node in child_el.children() {
                                match row_node.value() {
                                    Node::Text(text) => {
                                        self.push_inline_text(text.text.as_ref(), style);
                                    }
                                    Node::Element(rel) => {
                                        if is_ignored_noise_tag(rel.name())
                                            || has_display_none(rel)
                                        {
                                            continue;
                                        }
                                        if let Some(row_el) = ElementRef::wrap(row_node) {
                                            if rel.name() == "tr" {
                                                self.linearize_table_row(row_el, style);
                                            } else {
                                                self.handle_element(row_el, style);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "tr" => {
                            self.linearize_table_row(child_el, style);
                        }
                        "caption" => {
                            self.flush_paragraph();
                            self.walk_children(child_el, style);
                            self.flush_paragraph();
                        }
                        _ => {
                            self.handle_element(child_el, style);
                        }
                    }
                }
                _ => {}
            }
        }
        self.flush_paragraph();
    }

    fn linearize_table_row(&mut self, row_el: ElementRef<'_>, style: &TextStyleFlags) {
        for cell_node in row_el.children() {
            match cell_node.value() {
                Node::Text(text) => {
                    self.push_inline_text(text.text.as_ref(), style);
                }
                Node::Element(cel) => {
                    if is_ignored_noise_tag(cel.name()) || has_display_none(cel) {
                        continue;
                    }
                    let Some(cell_el) = ElementRef::wrap(cell_node) else {
                        continue;
                    };
                    if matches!(cel.name(), "td" | "th") {
                        self.flush_paragraph();
                        self.walk_children(cell_el, style);
                        self.flush_paragraph();
                    } else {
                        self.handle_element(cell_el, style);
                    }
                }
                _ => {}
            }
        }
    }

    fn render_data_table(&mut self, table_el: ElementRef<'_>, style: &TextStyleFlags) {
        let mut raw_rows: Vec<(bool, ElementRef<'_>)> = Vec::new();
        for child in table_el.children() {
            let Some(child_el) = ElementRef::wrap(child) else {
                continue;
            };
            let el = child_el.value();
            if is_ignored_noise_tag(el.name()) || has_display_none(el) {
                continue;
            }
            match el.name() {
                "caption" => {
                    self.flush_paragraph();
                    let mut cap_style = style.clone();
                    cap_style.bold = true;
                    let spans = self.collect_inline_spans(child_el, &cap_style);
                    if !spans_are_blank(&spans) {
                        self.blocks.push(RichBlock::Paragraph(trim_spans(spans)));
                    }
                }
                "thead" => {
                    for row_el in child_el.children().filter_map(ElementRef::wrap) {
                        if row_el.value().name() == "tr" && !has_display_none(row_el.value()) {
                            raw_rows.push((true, row_el));
                        }
                    }
                }
                "tbody" | "tfoot" => {
                    for row_el in child_el.children().filter_map(ElementRef::wrap) {
                        if row_el.value().name() == "tr" && !has_display_none(row_el.value()) {
                            raw_rows.push((false, row_el));
                        }
                    }
                }
                "tr" => {
                    raw_rows.push((false, child_el));
                }
                _ => {}
            }
        }

        let mut table_spans: Vec<RichSpan> = Vec::new();
        let mut rendered_rows = 0usize;
        for (in_thead, row_el) in raw_rows {
            let mut cells: Vec<(bool, Vec<RichSpan>)> = Vec::new();
            for cell_el in row_el.children().filter_map(ElementRef::wrap) {
                let cel = cell_el.value();
                if has_display_none(cel) {
                    continue;
                }
                let is_th = cel.name() == "th";
                if is_th || cel.name() == "td" {
                    let mut cell_style = style.clone();
                    if in_thead || is_th {
                        cell_style.bold = true;
                    }
                    let spans = trim_spans(self.collect_inline_spans(cell_el, &cell_style));
                    cells.push((in_thead || is_th, spans));
                }
            }
            if cells.is_empty() || cells.iter().all(|(_, s)| spans_are_blank(s)) {
                continue;
            }
            if rendered_rows > 0 {
                table_spans.push(RichSpan::plain("\n"));
            }
            for (ci, (_, cell_spans)) in cells.into_iter().enumerate() {
                if ci > 0 {
                    table_spans.push(RichSpan::plain(" │ "));
                }
                for mut sp in cell_spans {
                    if sp.text.contains('\n') {
                        sp.text = sp.text.replace('\n', " ");
                    }
                    if !sp.text.is_empty() {
                        table_spans.push(sp);
                    }
                }
            }
            rendered_rows += 1;
        }

        if !spans_are_blank(&table_spans) {
            self.blocks.push(RichBlock::Paragraph(trim_spans(table_spans)));
        }
    }

    fn collect_inline_spans(
        &mut self,
        el_ref: ElementRef<'_>,
        style: &TextStyleFlags,
    ) -> Vec<RichSpan> {
        let saved = std::mem::take(&mut self.current_spans);
        self.walk_children(el_ref, style);
        std::mem::replace(&mut self.current_spans, saved)
    }

    fn collect_quote_spans(
        &mut self,
        el_ref: ElementRef<'_>,
        style: &TextStyleFlags,
    ) -> Vec<RichSpan> {
        let saved = std::mem::take(&mut self.current_spans);
        self.walk_quote_children(el_ref, style);
        std::mem::replace(&mut self.current_spans, saved)
    }

    fn walk_quote_children(&mut self, parent: ElementRef<'_>, style: &TextStyleFlags) {
        for child in parent.children() {
            match child.value() {
                Node::Text(text) => {
                    self.push_inline_text(text.text.as_ref(), style);
                }
                Node::Element(el) => {
                    if el.attr("itemprop") == Some("time")
                        || is_ignored_noise_tag(el.name())
                        || has_display_none(el)
                    {
                        continue;
                    }
                    let Some(child_el) = ElementRef::wrap(child) else {
                        continue;
                    };
                    match el.name() {
                        "p" | "div" => {
                            if !self.current_spans.is_empty() {
                                self.push_raw_span("\n", &TextStyleFlags::default());
                            }
                            self.walk_quote_children(child_el, style);
                        }
                        "br" => {
                            self.push_raw_span("\n", &TextStyleFlags::default());
                        }
                        "b" | "strong" => {
                            let mut next = style.clone();
                            next.bold = true;
                            self.walk_quote_children(child_el, &next);
                        }
                        "i" | "em" => {
                            let mut next = style.clone();
                            next.italic = true;
                            self.walk_quote_children(child_el, &next);
                        }
                        "code" => {
                            let mut next = style.clone();
                            next.code = true;
                            let raw = collect_raw_text(child_el);
                            if !raw.is_empty() {
                                self.push_raw_span(&raw, &next);
                            }
                        }
                        _ => {
                            self.walk_quote_children(child_el, style);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn push_inline_text(&mut self, raw: &str, style: &TextStyleFlags) {
        let collapsed = collapse_html_whitespace(raw);
        if collapsed.is_empty() {
            return;
        }
        let to_push = if collapsed == " "
            && (self.current_spans.is_empty()
                || self
                    .current_spans
                    .last()
                    .map(|s| s.text.ends_with('\n') || s.text.ends_with(' '))
                    .unwrap_or(false))
        {
            return;
        } else if self.current_spans.is_empty() {
            collapsed.trim_start().to_string()
        } else {
            collapsed
        };

        if !to_push.is_empty() {
            self.push_raw_span(&to_push, style);
        }
    }

    fn push_raw_span(&mut self, text: &str, style: &TextStyleFlags) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.current_spans.last_mut() {
            if &last.style == style {
                last.text.push_str(text);
                return;
            }
        }
        self.current_spans.push(RichSpan {
            text: text.to_string(),
            style: style.clone(),
        });
    }

    fn flush_paragraph(&mut self) {
        let spans = std::mem::take(&mut self.current_spans);
        if !spans_are_blank(&spans) {
            self.blocks.push(RichBlock::Paragraph(trim_spans(spans)));
        }
    }

    fn push_attachment_block(&mut self, att: &AttachmentInfo) {
        if is_card_content_type(&att.content_type) {
            if let Some(card) = parse_card_attachment(att) {
                self.blocks.push(card);
                return;
            }
        }
        self.blocks.push(RichBlock::AttachmentChip {
            id: att.id.clone(),
            name: att.name.clone(),
        });
    }

    fn is_seen_inline_attachment(&self, att: &AttachmentInfo) -> bool {
        if let Some(url) = att.content_url.as_deref() {
            if self.seen_inline_srcs.contains(url) {
                return true;
            }
            if let Some(hid) = extract_hosted_content_id(url) {
                if self.seen_hosted_ids.contains(hid) {
                    return true;
                }
            }
        }
        let att_hid = att.id.strip_prefix("hosted:").unwrap_or(&att.id);
        self.seen_hosted_ids.contains(att_hid)
    }
}

pub(crate) fn extract_hosted_content_id(url: &str) -> Option<&str> {
    let rest = url
        .split_once("/hostedContents/")
        .map(|(_, r)| r)
        .or_else(|| url.strip_prefix("hostedContents/"))?;
    let seg = rest.split(['/', '?', '#']).next()?.trim();
    if seg.is_empty() {
        None
    } else {
        Some(seg)
    }
}

fn non_empty_attr<'a>(el: &'a scraper::node::Element, name: &str) -> Option<&'a str> {
    el.attr(name).map(str::trim).filter(|s| !s.is_empty())
}

fn is_emoji_sprite_img(el: &scraper::node::Element) -> bool {
    if el
        .attr("itemtype")
        .is_some_and(|t| t.to_ascii_lowercase().contains("emoji"))
    {
        return true;
    }
    if let Some(src) = el.attr("src") {
        let lower = src.to_ascii_lowercase();
        if lower.contains("statics.teams.cdn.office.net")
            || lower.contains("/personal-expressions/")
            || lower.contains("/emoticons/")
        {
            return true;
        }
    }
    false
}

fn detect_code_lang(pre_el: ElementRef<'_>) -> Option<String> {
    let el = pre_el.value();
    if let Some(lang) = el
        .attr("lang")
        .or_else(|| el.attr("data-lang"))
        .and_then(non_empty)
    {
        return Some(lang);
    }
    if let Some(lang) = lang_from_class(el.attr("class")) {
        return Some(lang);
    }
    for child in pre_el.children() {
        if let Node::Element(child_el) = child.value() {
            if child_el.name() == "code" {
                if let Some(lang) = child_el.attr("lang").and_then(non_empty) {
                    return Some(lang);
                }
                if let Some(lang) = lang_from_class(child_el.attr("class")) {
                    return Some(lang);
                }
            }
        }
    }
    None
}

fn lang_from_class(class_attr: Option<&str>) -> Option<String> {
    let classes = class_attr?;
    for cls in classes.split_whitespace() {
        if let Some(rest) = cls
            .strip_prefix("language-")
            .or_else(|| cls.strip_prefix("lang-"))
        {
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn is_ignored_noise_tag(tag: &str) -> bool {
    matches!(
        tag,
        "script"
            | "style"
            | "head"
            | "meta"
            | "title"
            | "noscript"
            | "template"
            | "xml"
            | "o:p"
            | "svg"
            | "canvas"
    )
}

fn has_display_none(el: &scraper::node::Element) -> bool {
    let Some(style) = el.attr("style") else {
        return false;
    };
    for decl in style.split(';') {
        let Some((prop, val)) = decl.split_once(':') else {
            continue;
        };
        let val_clean = val.split('!').next().unwrap_or("").trim();
        if prop.trim().eq_ignore_ascii_case("display") && val_clean.eq_ignore_ascii_case("none") {
            return true;
        }
    }
    false
}

fn is_presentation_role(el: &scraper::node::Element) -> bool {
    el.attr("role").is_some_and(|r| {
        let trimmed = r.trim();
        trimmed.eq_ignore_ascii_case("presentation") || trimmed.eq_ignore_ascii_case("none")
    })
}

fn has_nested_table(table_el: ElementRef<'_>) -> bool {
    table_el
        .descendants()
        .skip(1)
        .any(|desc| ElementRef::wrap(desc).is_some_and(|el| el.value().name() == "table"))
}

fn row_has_th_or_block_td(row_el: ElementRef<'_>, has_th: &mut bool, has_block_td: &mut bool) {
    for cell in row_el.children().filter_map(ElementRef::wrap) {
        match cell.value().name() {
            "th" => *has_th = true,
            "td" if cell.descendants().skip(1).any(|desc| {
                ElementRef::wrap(desc).is_some_and(|el| {
                    matches!(
                        el.value().name(),
                        "div"
                            | "p"
                            | "ul"
                            | "ol"
                            | "blockquote"
                            | "pre"
                            | "h1"
                            | "h2"
                            | "h3"
                            | "h4"
                            | "h5"
                            | "h6"
                            | "hr"
                            | "table"
                    )
                })
            }) =>
            {
                *has_block_td = true;
            }
            _ => {}
        }
    }
}

fn count_row_columns(row_el: ElementRef<'_>) -> usize {
    let mut cols = 0usize;
    for cell in row_el.children().filter_map(ElementRef::wrap) {
        if matches!(cell.value().name(), "td" | "th") {
            let span = cell
                .value()
                .attr("colspan")
                .and_then(|s| s.trim().parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(1);
            cols += span;
        }
    }
    cols
}

fn should_linearize_table(table_el: ElementRef<'_>) -> bool {
    if is_presentation_role(table_el.value()) || has_nested_table(table_el) {
        return true;
    }

    let mut max_cols = 0usize;
    let mut has_th_or_thead = false;
    let mut has_block_td = false;
    for child in table_el.children() {
        let Some(child_el) = ElementRef::wrap(child) else {
            continue;
        };
        if is_presentation_role(child_el.value()) {
            return true;
        }
        match child_el.value().name() {
            "thead" | "tbody" | "tfoot" => {
                if child_el.value().name() == "thead" {
                    has_th_or_thead = true;
                }
                for row_el in child_el.children().filter_map(ElementRef::wrap) {
                    if row_el.value().name() == "tr" {
                        if is_presentation_role(row_el.value()) {
                            return true;
                        }
                        max_cols = max_cols.max(count_row_columns(row_el));
                        row_has_th_or_block_td(row_el, &mut has_th_or_thead, &mut has_block_td);
                    }
                }
            }
            "tr" => {
                max_cols = max_cols.max(count_row_columns(child_el));
                row_has_th_or_block_td(child_el, &mut has_th_or_thead, &mut has_block_td);
            }
            _ => {}
        }
    }

    max_cols <= 1 || (!has_th_or_thead && has_block_td)
}

fn collect_raw_text(el_ref: ElementRef<'_>) -> String {
    let mut out = String::new();
    collect_raw_text_inner(el_ref, &mut out);
    out
}

fn collect_raw_text_inner(el_ref: ElementRef<'_>, out: &mut String) {
    for child in el_ref.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t.text.as_ref()),
            Node::Element(el) => {
                if is_ignored_noise_tag(el.name()) || has_display_none(el) {
                    continue;
                }
                if el.name() == "br" {
                    out.push('\n');
                } else if let Some(child_el) = ElementRef::wrap(child) {
                    collect_raw_text_inner(child_el, out);
                    if matches!(el.name(), "p" | "div") && !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
}

fn collapse_inline_spaces(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_space = false;
    for ch in input.chars() {
        if ch == ' ' || ch == '\t' {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out
}

fn collapse_html_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_ws = false;
    for ch in input.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

fn spans_are_blank(spans: &[RichSpan]) -> bool {
    spans.iter().all(|s| s.text.trim().is_empty())
}

fn trim_spans(mut spans: Vec<RichSpan>) -> Vec<RichSpan> {
    let start_idx = spans
        .iter()
        .position(|s| !s.text.trim_start_matches([' ', '\t', '\n', '\r']).is_empty())
        .unwrap_or(spans.len());
    if start_idx > 0 {
        spans.drain(..start_idx);
    }
    if let Some(first) = spans.first_mut() {
        let trimmed_len = first.text.trim_start_matches([' ', '\t', '\n', '\r']).len();
        let lead = first.text.len() - trimmed_len;
        if lead > 0 {
            first.text.drain(..lead);
        }
    }
    while let Some(last) = spans.last_mut() {
        let trimmed_len = last.text.trim_end_matches([' ', '\t', '\n', '\r']).len();
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

fn spans_to_plain(spans: &[RichSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

fn spans_to_markdown(spans: &[RichSpan]) -> String {
    let total_len: usize = spans.iter().map(|s| s.text.len()).sum();
    let mut out = String::with_capacity(total_len);
    for span in spans {
        if span.text.is_empty() {
            continue;
        }
        let has_link = span.style.link_url.is_some();
        if has_link {
            out.push('[');
        }
        if !span.style.code {
            if span.style.underline {
                out.push_str("<u>");
            }
            if span.style.strike {
                out.push_str("~~");
            }
            if span.style.italic {
                out.push('*');
            }
            if span.style.bold {
                out.push_str("**");
            }
            out.push_str(&span.text);
            if span.style.bold {
                out.push_str("**");
            }
            if span.style.italic {
                out.push('*');
            }
            if span.style.strike {
                out.push_str("~~");
            }
            if span.style.underline {
                out.push_str("</u>");
            }
        } else if span.text.contains('`') {
            out.push_str("`` ");
            out.push_str(&span.text);
            out.push_str(" ``");
        } else {
            out.push('`');
            out.push_str(&span.text);
            out.push('`');
        }
        if let Some(url) = &span.style.link_url {
            out.push_str("](");
            out.push_str(url);
            out.push(')');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_attachment(id: &str, name: &str) -> AttachmentInfo {
        AttachmentInfo {
            id: id.to_string(),
            content_type: "reference".to_string(),
            content_url: Some(format!("https://example.com/{name}")),
            name: Some(name.to_string()),
            content_json: None,
            thumbnail_url: None,
        }
    }

    fn sample_mention(id: i32, user_id: &str, display_name: &str) -> MentionInfo {
        MentionInfo {
            id,
            user_id: user_id.to_string(),
            display_name: display_name.to_string(),
        }
    }

    #[test]
    fn parse_message_blocks_cache_hit_matches_uncached() {
        let html = "<p>Hello <b>Teams</b> <at id=\"0\"></at></p>";
        let atts = vec![sample_attachment("a1", "spec.pdf")];
        let mentions = vec![sample_mention(0, "u1", "Alice")];

        let expected = parse_message_blocks_uncached(html, &atts, &mentions);
        let first = parse_message_blocks(html, &atts, &mentions);
        let second = parse_message_blocks(html, &atts, &mentions);

        assert_eq!(first, expected);
        assert_eq!(second, expected);
    }

    #[test]
    fn parse_message_blocks_cache_distinguishes_attachments_and_mentions() {
        let html = "<p>Check <at id=\"0\"></at></p>";
        let att_a = vec![sample_attachment("a1", "alpha.txt")];
        let att_b = vec![sample_attachment("a1", "beta.txt")];
        let men_a = vec![sample_mention(0, "u1", "Alice")];
        let men_b = vec![sample_mention(0, "u2", "Bob")];

        assert_ne!(
            compute_parse_fingerprint(html, &att_a, &men_a),
            compute_parse_fingerprint(html, &att_b, &men_a)
        );
        assert_ne!(
            compute_parse_fingerprint(html, &att_a, &men_a),
            compute_parse_fingerprint(html, &att_a, &men_b)
        );

        let res_a = parse_message_blocks(html, &att_a, &men_a);
        let res_b = parse_message_blocks(html, &att_b, &men_b);
        assert_ne!(res_a, res_b);
        assert!(blocks_to_plain(&res_a).contains("@Alice"));
        assert!(blocks_to_plain(&res_a).contains("alpha.txt"));
        assert!(blocks_to_plain(&res_b).contains("@Bob"));
        assert!(blocks_to_plain(&res_b).contains("beta.txt"));
    }

    #[test]
    fn parse_message_blocks_skips_cache_for_huge_html() {
        let big_text = "x".repeat(HTML_CACHE_MAX_HTML_BYTES + 64);
        let html = format!("<p>{big_text}</p>");
        let blocks = parse_message_blocks(&html, &[], &[]);
        assert_eq!(blocks.len(), 1);
    }

    #[test]
    fn html_cache_evicts_at_max_capacity() {
        let mut cache = HtmlParseCache::default();
        for i in 0..(HTML_CACHE_MAX_ENTRIES + 50) {
            cache.insert((i as u64, i), Arc::from(Vec::new()));
        }
        assert_eq!(cache.len(), HTML_CACHE_MAX_ENTRIES);
        assert_eq!(cache.nodes.len(), HTML_CACHE_MAX_ENTRIES);
        assert!(cache.get(&(0, 0)).is_none());
        let last = HTML_CACHE_MAX_ENTRIES + 49;
        assert!(cache.get(&(last as u64, last)).is_some());
    }

    #[test]
    fn html_cache_promotes_on_get_lru() {
        let mut cache = HtmlParseCache::default();
        for i in 0..HTML_CACHE_MAX_ENTRIES {
            cache.insert((i as u64, i), Arc::from(Vec::new()));
        }
        // Promote key 0 to MRU before inserting one new item; key 1 should be evicted instead of 0.
        assert!(cache.get(&(0, 0)).is_some());
        cache.insert((99999, 99999), Arc::from(Vec::new()));
        assert!(cache.get(&(0, 0)).is_some());
        assert!(cache.get(&(1, 1)).is_none());
    }

    #[test]
    fn emoji_sprites_render_inline_text_without_image_chips() {
        let html = r#"<p>Ship it <emoji alt="🚀"><img itemtype="http://schema.skype.com/Emoji" src="https://statics.teams.cdn.office.net/evergreen-assets/personal-expressions/v2/assets/emoticons/rocket/default/50_f.png" alt="🚀"></emoji> now <img itemtype="http://schema.skype.com/Emoji" src="https://example.com/e.png" alt="👍"> and <img src="https://statics.teams.cdn.office.net/evergreen-assets/personal-expressions/v2/assets/emoticons/smile/default/50_f.png" title="😊"></p>"#;
        let blocks = parse_message_blocks(html, &[], &[]);
        assert!(
            !blocks
                .iter()
                .any(|b| matches!(b, RichBlock::InlineImageChip { .. })),
            "emoji sprites must not produce InlineImageChip: {blocks:?}"
        );
        let plain = blocks_to_plain(&blocks);
        assert_eq!(plain, "Ship it 🚀 now 👍 and 😊");
    }

    #[test]
    fn emoji_without_wrapper_alt_falls_back_to_child_img_alt() {
        let html = r#"<p>Done <emoji><img itemtype="http://schema.skype.com/Emoji" src="https://statics.teams.cdn.office.net/e.png" alt="✅"></emoji></p>"#;
        let blocks = parse_message_blocks(html, &[], &[]);
        assert!(blocks
            .iter()
            .all(|b| !matches!(b, RichBlock::InlineImageChip { .. })));
        assert_eq!(blocks_to_plain(&blocks), "Done ✅");
    }

    #[test]
    fn legacy_reply_blockquote_separates_author_and_preview() {
        let legacy_html = r#"<blockquote itemscope="" itemtype="http://schema.skype.com/Reply" itemid="c-msg-1"><strong itemprop="mri" itemid="u-alice">Alice Kim</strong><span itemprop="time" itemid="c-msg-1">11/20/2026 8:00 AM</span><p itemprop="preview">Check the rollout plan</p></blockquote><p>Legacy quote reply</p>"#;
        assert_eq!(
            crate::parser::html_and_cards_to_plain(legacy_html, &[]),
            "> Alice Kim\n> Check the rollout plan\nLegacy quote reply"
        );

        let p_only_html = "<blockquote><p>Only</p></blockquote><p>Body</p>";
        assert_eq!(
            crate::parser::html_and_cards_to_plain(p_only_html, &[]),
            "> Only\nBody"
        );
    }

    #[test]
    fn message_reference_attachment_renders_no_chip() {
        let msg_ref = AttachmentInfo {
            id: "c-msg-1".to_string(),
            content_type: "messageReference".to_string(),
            content_url: None,
            name: None,
            content_json: Some(
                r#"{"messageId":"c-msg-1","messagePreview":"Check the rollout plan","messageSender":{"user":{"id":"u-alice","displayName":"Alice Kim"}}}"#
                    .to_string(),
            ),
            thumbnail_url: None,
        };
        let html = r#"<attachment id="c-msg-1"></attachment><p>Looks good</p>"#;

        assert_eq!(
            crate::parser::html_and_cards_to_plain(html, std::slice::from_ref(&msg_ref)),
            "Looks good"
        );

        let with_file = vec![msg_ref, sample_attachment("f1", "spec.pdf")];
        let plain_with_file = crate::parser::html_and_cards_to_plain(html, &with_file);
        assert!(plain_with_file.contains("[Attachment: spec.pdf]"), "{plain_with_file}");
        assert!(!plain_with_file.contains("c-msg-1"), "{plain_with_file}");

        assert_eq!(
            crate::parser::html_and_cards_to_plain(r#"<attachment id="x9"></attachment>"#, &[]),
            "[Attachment: x9]"
        );
    }

    #[test]
    fn blockquote_strips_noise_tags_svg_and_display_none() {
        let html = r#"<blockquote><style>.x{color:red}</style><script>bad()</script><svg><desc>secret</desc></svg><p style="display:none">hidden</p><p>Visible</p></blockquote>"#;
        let blocks = parse_message_blocks(html, &[], &[]);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            RichBlock::Quote(spans) => {
                let text = spans_to_plain(spans);
                assert_eq!(text, "Visible");
            }
            other => panic!("expected Quote block, got {other:?}"),
        }
        let plain = blocks_to_plain(&blocks);
        assert_eq!(plain, "> Visible");
        for forbidden in [".x", "color:red", "bad()", "secret", "hidden"] {
            assert!(!plain.contains(forbidden), "leaked {forbidden} in {plain}");
        }
    }

    #[test]
    fn handle_element_strips_noise_tags_and_handles_layout_and_data_tables() {
        let html = r#"
            <head><title>DocTitle</title><meta charset="utf-8"></head>
            <noscript>no-js</noscript>
            <template>tpl-secret</template>
            <xml><o:p>office-meta</o:p></xml>
            <svg><desc>svg-secret</desc><rect width="10" height="10"/></svg>
            <canvas>canvas-fallback</canvas>
            <div style="display: none !important">hidden-div</div>
            <table role="presentation"><tr><td><p>Layout One</p></td></tr><tr><td><p>Layout Two</p></td></tr></table>
            <table><thead><tr><th>Name</th><th>Status</th></tr></thead><tbody><tr><td>API</td><td>OK</td></tr></tbody></table>
        "#;
        let blocks = parse_message_blocks(html, &[], &[]);
        let plain = blocks_to_plain(&blocks);
        for forbidden in [
            "DocTitle",
            "no-js",
            "tpl-secret",
            "office-meta",
            "svg-secret",
            "canvas-fallback",
            "hidden-div",
        ] {
            assert!(!plain.contains(forbidden), "leaked {forbidden} in:\n{plain}");
        }
        assert!(plain.contains("Layout One"), "{plain}");
        assert!(plain.contains("Layout Two"), "{plain}");
        assert!(plain.contains("Name │ Status"), "{plain}");
        assert!(plain.contains("API │ OK"), "{plain}");
    }

    #[test]
    fn html_ol_and_hr_render_cleanly() {
        let html = r#"<p>Before</p><ol><li>First <b>step</b></li><li>Second</li></ol><hr><ol start="5"><li>Fifth</li></ol>"#;
        let blocks = parse_message_blocks(html, &[], &[]);
        let plain = blocks_to_plain(&blocks);
        assert!(plain.contains("1. First step\n2. Second"), "{plain}");
        assert!(plain.contains("────"), "{plain}");
        assert!(plain.contains("5. Fifth"), "{plain}");
    }

    #[test]
    fn plain_text_markdown_preserves_newlines_and_parses_formatting() {
        let md = "Line 1\nLine 2\n\n- **item**\n- `code`";
        let blocks = parse_message_blocks(md, &[], &[]);
        let plain = blocks_to_plain(&blocks);
        assert!(plain.contains("Line 1\nLine 2"), "newline collapsed:\n{plain}");
        assert!(plain.contains("- item"), "{plain}");
        assert!(plain.contains("- code"), "{plain}");
        assert!(!plain.contains("**"), "raw ** leaked:\n{plain}");
    }

    #[test]
    fn plain_text_with_ampersand_preserves_newlines_without_collapsing() {
        let input = "R&D team update\nTom &amp; Jerry\nFinal line";
        let blocks = parse_message_blocks(input, &[], &[]);
        let plain = blocks_to_plain(&blocks);
        assert_eq!(plain, "R&D team update\nTom & Jerry\nFinal line");
    }
}
