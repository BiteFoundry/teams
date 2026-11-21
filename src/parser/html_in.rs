//! Teams HTML + attachment parser into `Vec<RichBlock>`, plain text, and GFM Markdown.

use std::collections::{hash_map::DefaultHasher, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use scraper::{ElementRef, Html, Node};

use crate::{
    models::{AttachmentInfo, MentionInfo, RichBlock, RichSpan, TextStyleFlags},
    parser::card_in::{is_card_content_type, parse_card_attachment},
};

const HTML_CACHE_MAX_ENTRIES: usize = 1024;
const HTML_CACHE_MAX_HTML_BYTES: usize = 256 * 1024;

type HtmlCacheKey = (u64, u64, usize);

#[derive(Default)]
struct HtmlParseCache {
    entries: HashMap<HtmlCacheKey, Arc<[RichBlock]>>,
    order: VecDeque<HtmlCacheKey>,
}

impl HtmlParseCache {
    fn get(&mut self, key: &HtmlCacheKey) -> Option<Arc<[RichBlock]>> {
        let val = self.entries.get(key).cloned()?;
        if self.order.back() != Some(key) {
            if let Some(pos) = self.order.iter().rposition(|k| k == key) {
                self.order.remove(pos);
            }
            self.order.push_back(*key);
        }
        Some(val)
    }

    fn insert(&mut self, key: HtmlCacheKey, value: Arc<[RichBlock]>) {
        if self.entries.contains_key(&key) {
            self.entries.insert(key, value);
            if self.order.back() != Some(&key) {
                if let Some(pos) = self.order.iter().rposition(|k| k == &key) {
                    self.order.remove(pos);
                }
                self.order.push_back(key);
            }
            return;
        }
        while self.entries.len() >= HTML_CACHE_MAX_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
        self.entries.insert(key, value);
        self.order.push_back(key);
    }
}

static HTML_PARSE_CACHE: OnceLock<Mutex<HtmlParseCache>> = OnceLock::new();

struct Fnv1a64(u64);

impl Fnv1a64 {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn finish_mixed(&self) -> u64 {
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
}

impl Hasher for Fnv1a64 {
    fn finish(&self) -> u64 {
        self.finish_mixed()
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

fn hash_str_field(h1: &mut DefaultHasher, h2: &mut Fnv1a64, s: &str) {
    s.len().hash(h1);
    s.as_bytes().hash(h1);
    s.len().hash(h2);
    s.as_bytes().hash(h2);
}

fn hash_opt_str_field(h1: &mut DefaultHasher, h2: &mut Fnv1a64, opt: Option<&str>) {
    match opt {
        Some(s) => {
            1u8.hash(h1);
            1u8.hash(h2);
            hash_str_field(h1, h2, s);
        }
        None => {
            0u8.hash(h1);
            0u8.hash(h2);
        }
    }
}

fn compute_parse_fingerprint(
    html: &str,
    attachments: &[AttachmentInfo],
    mentions: &[MentionInfo],
) -> HtmlCacheKey {
    let mut h1 = DefaultHasher::new();
    let mut h2 = Fnv1a64::new();

    0x4854_4d4c_u32.hash(&mut h1);
    0x9e37_79b9_u32.hash(&mut h2);

    hash_str_field(&mut h1, &mut h2, html);

    attachments.len().hash(&mut h1);
    attachments.len().hash(&mut h2);
    for att in attachments {
        hash_str_field(&mut h1, &mut h2, &att.id);
        hash_str_field(&mut h1, &mut h2, &att.content_type);
        hash_opt_str_field(&mut h1, &mut h2, att.name.as_deref());
        hash_opt_str_field(&mut h1, &mut h2, att.content_url.as_deref());
        hash_opt_str_field(&mut h1, &mut h2, att.content_json.as_deref());
        hash_opt_str_field(&mut h1, &mut h2, att.thumbnail_url.as_deref());
    }

    mentions.len().hash(&mut h1);
    mentions.len().hash(&mut h2);
    for m in mentions {
        m.id.hash(&mut h1);
        m.id.hash(&mut h2);
        m.user_id.hash(&mut h1);
        m.user_id.hash(&mut h2);
        hash_str_field(&mut h1, &mut h2, &m.display_name);
    }

    (h1.finish(), h2.finish(), html.len())
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
    if !html.contains('<') && !html.contains('&') && !html.contains('\0') {
        let collapsed = collapse_html_whitespace(html);
        let trimmed = collapsed.trim();
        if trimmed.is_empty() {
            return Some(Arc::from([]));
        }
        return Some(Arc::from([RichBlock::Paragraph(vec![RichSpan {
            text: trimmed.to_string(),
            style: TextStyleFlags::default(),
        }])]));
    }
    None
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
        let fragment = Html::parse_fragment(html);
        ctx.walk_children(fragment.root_element(), &TextStyleFlags::default());
        ctx.flush_paragraph();
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
                let mut lines = Vec::new();
                if let Some(t) = title.as_deref().filter(|s| !s.is_empty()) {
                    lines.push(t.to_string());
                }
                if let Some(sub) = subtitle.as_deref().filter(|s| !s.is_empty()) {
                    lines.push(sub.to_string());
                }
                for bl in body_lines {
                    if !bl.is_empty() {
                        lines.push(bl.clone());
                    }
                }
                for (k, v) in facts {
                    if k.is_empty() {
                        lines.push(v.clone());
                    } else {
                        lines.push(format!("{k}: {v}"));
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
            "ul" | "ol" => {
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
            "script" | "style" => {}
            _ => {
                self.walk_children(el_ref, style);
            }
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
                    if el.attr("itemprop") == Some("time") {
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
            cache.insert((i as u64, (i as u64) ^ 0x55, i), Arc::from(Vec::new()));
        }
        assert_eq!(cache.entries.len(), HTML_CACHE_MAX_ENTRIES);
        assert_eq!(cache.order.len(), HTML_CACHE_MAX_ENTRIES);
        assert!(cache.get(&(0, 0x55, 0)).is_none());
        let last = HTML_CACHE_MAX_ENTRIES + 49;
        assert!(cache.get(&(last as u64, (last as u64) ^ 0x55, last)).is_some());
    }

    #[test]
    fn html_cache_promotes_on_get_lru() {
        let mut cache = HtmlParseCache::default();
        for i in 0..HTML_CACHE_MAX_ENTRIES {
            cache.insert((i as u64, 1, i), Arc::from(Vec::new()));
        }
        // Promote key 0 to MRU before inserting one new item; key 1 should be evicted instead of 0.
        assert!(cache.get(&(0, 1, 0)).is_some());
        cache.insert((99999, 1, 99999), Arc::from(Vec::new()));
        assert!(cache.get(&(0, 1, 0)).is_some());
        assert!(cache.get(&(1, 1, 1)).is_none());
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
}
