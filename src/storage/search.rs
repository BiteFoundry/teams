//! Hybrid FTS5 (`trigram` / `unicode61`) + `LIKE` message search with contextual Korean-safe snippets.

use anyhow::Result;
use rusqlite::{params, Connection, Row};

use crate::{
    models::{ConversationKind, SearchHit},
    storage::repo::escape_like,
};

struct SearchQueryContext {
    query_lower: String,
    tokens_lower: Vec<String>,
}

impl SearchQueryContext {
    fn new(trimmed_query: &str) -> Self {
        let query_lower = trimmed_query.to_lowercase();
        let tokens_lower = query_lower
            .split_whitespace()
            .map(str::to_string)
            .collect();
        Self {
            query_lower,
            tokens_lower,
        }
    }
}

pub fn search_messages(
    conn: &Connection,
    query: &str,
    conv_key_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let trimmed = query.trim();
    if trimmed.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
        return Ok(Vec::new());
    }

    let query_ctx = SearchQueryContext::new(trimmed);
    let all_tokens_at_least_3 = tokens.iter().all(|t| t.chars().count() >= 3);
    if all_tokens_at_least_3 {
        if let Some(fts_query) =
            build_fts_query_from_tokens(trimmed, &tokens, all_tokens_at_least_3)
        {
            if let Ok(hits) =
                run_fts_search(conn, &fts_query, &query_ctx, conv_key_filter, limit)
            {
                return Ok(hits);
            }
        }
    }

    run_like_search(conn, &query_ctx, conv_key_filter, limit)
}

fn build_fts_query_from_tokens(
    trimmed: &str,
    tokens: &[&str],
    all_tokens_at_least_3: bool,
) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    // In FTS5 trigram mode, every token in an AND query needs >= 3 chars to match.
    // If any token is < 3 chars, try exact phrase if overall >= 3 chars; if 0 hits, LIKE handles it.
    if tokens.len() > 1 && all_tokens_at_least_3 {
        let quoted: Vec<String> = tokens
            .iter()
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        Some(quoted.join(" "))
    } else {
        Some(format!("\"{}\"", trimmed.replace('"', "\"\"")))
    }
}

fn run_fts_search(
    conn: &Connection,
    fts_query: &str,
    query_ctx: &SearchQueryContext,
    conv_key_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let hits = if let Some(conv_key) = conv_key_filter {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                m.conv_key,
                COALESCE(c.title, m.conv_key) AS conv_title,
                c.kind,
                m.msg_id,
                m.parent_msg_id,
                m.author_name,
                m.created_at,
                m.body_plain,
                m.subject
            FROM messages_fts f
            JOIN messages m ON m.msg_key = f.msg_key
            LEFT JOIN conversations c ON c.conv_key = m.conv_key
            WHERE messages_fts MATCH ?1
              AND m.deleted = 0
              AND m.conv_key = ?2
            ORDER BY m.created_at_sort DESC, m.msg_id DESC
            LIMIT ?3
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, conv_key, limit as i64], |row| {
            map_search_hit_row(row, query_ctx)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let mut stmt = conn.prepare_cached(
            "
            SELECT
                m.conv_key,
                COALESCE(c.title, m.conv_key) AS conv_title,
                c.kind,
                m.msg_id,
                m.parent_msg_id,
                m.author_name,
                m.created_at,
                m.body_plain,
                m.subject
            FROM messages_fts f
            JOIN messages m ON m.msg_key = f.msg_key
            LEFT JOIN conversations c ON c.conv_key = m.conv_key
            WHERE messages_fts MATCH ?1
              AND m.deleted = 0
            ORDER BY m.created_at_sort DESC, m.msg_id DESC
            LIMIT ?2
            ",
        )?;
        let rows = stmt.query_map(params![fts_query, limit as i64], |row| {
            map_search_hit_row(row, query_ctx)
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    Ok(hits)
}

fn run_like_search(
    conn: &Connection,
    query_ctx: &SearchQueryContext,
    conv_key_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let mut sql = String::from(
        "SELECT
            m.conv_key,
            COALESCE(c.title, m.conv_key) AS conv_title,
            c.kind,
            m.msg_id,
            m.parent_msg_id,
            m.author_name,
            m.created_at,
            m.body_plain,
            m.subject
        FROM messages m
        LEFT JOIN conversations c ON c.conv_key = m.conv_key
        WHERE m.deleted = 0",
    );
    let mut bind_values: Vec<rusqlite::types::Value> = Vec::new();
    if let Some(conv_key) = conv_key_filter {
        bind_values.push(rusqlite::types::Value::Text(conv_key.to_string()));
        sql.push_str(&format!(" AND m.conv_key = ?{}", bind_values.len()));
    }
    for token in &query_ctx.tokens_lower {
        let pattern = format!("%{}%", escape_like(token));
        bind_values.push(rusqlite::types::Value::Text(pattern));
        let p = bind_values.len();
        sql.push_str(&format!(
            " AND (
                LOWER(m.body_plain) LIKE ?{p} ESCAPE '\\'
                OR LOWER(COALESCE(m.subject, '')) LIKE ?{p} ESCAPE '\\'
                OR LOWER(m.author_name) LIKE ?{p} ESCAPE '\\'
            )"
        ));
    }
    bind_values.push(rusqlite::types::Value::Integer(limit as i64));
    sql.push_str(&format!(
        " ORDER BY m.created_at_sort DESC, m.msg_id DESC LIMIT ?{}",
        bind_values.len()
    ));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(bind_values), |row| {
        map_search_hit_row(row, query_ctx)
    })?;
    let hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(hits)
}

fn map_search_hit_row(row: &Row<'_>, query_ctx: &SearchQueryContext) -> rusqlite::Result<SearchHit> {
    let conv_key: String = row.get(0)?;
    let conv_title: String = row.get(1)?;
    let kind_opt: Option<String> = row.get(2)?;
    let kind = match kind_opt.as_deref() {
        Some(k) => ConversationKind::from_db_str(k),
        None if conv_key.starts_with("chan:") => ConversationKind::Channel,
        None => ConversationKind::Group,
    };
    let msg_id: String = row.get(3)?;
    let parent_msg_id: Option<String> = row.get(4)?;
    let author_name: String = row.get(5)?;
    let created_at: String = row.get(6)?;
    let body_plain: String = row.get(7)?;
    let subject: Option<String> = row.get(8)?;

    let snippet = build_contextual_snippet(
        subject.as_deref(),
        &body_plain,
        &author_name,
        query_ctx,
        120,
    );

    Ok(SearchHit {
        conv_key,
        conv_title,
        kind,
        msg_id,
        parent_msg_id,
        author_name,
        created_at,
        snippet,
    })
}

fn normalize_whitespace(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for word in input.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn build_contextual_snippet(
    subject: Option<&str>,
    body_plain: &str,
    author_name: &str,
    query_ctx: &SearchQueryContext,
    max_chars: usize,
) -> String {
    let clean_body = normalize_whitespace(body_plain);
    let source = match subject.map(str::trim).filter(|s| !s.is_empty()) {
        Some(subj) if !clean_body.is_empty() => format!("[{subj}] {clean_body}"),
        Some(subj) => format!("[{subj}]"),
        None if !clean_body.is_empty() => clean_body,
        None => author_name.to_string(),
    };

    let total_chars = source.chars().count();
    if total_chars <= max_chars {
        return source;
    }

    let source_lower = source.to_lowercase();
    let match_char_idx = find_lower_char_pos(&source_lower, &query_ctx.query_lower)
        .or_else(|| {
            query_ctx
                .tokens_lower
                .iter()
                .filter_map(|tok| find_lower_char_pos(&source_lower, tok))
                .min()
        })
        .unwrap_or(0);

    let half = max_chars / 2;
    let mut start = match_char_idx.saturating_sub(half);
    let end = (start + max_chars).min(total_chars);
    if end - start < max_chars {
        start = end.saturating_sub(max_chars);
    }

    let start_byte = source
        .char_indices()
        .nth(start)
        .map(|(b, _)| b)
        .unwrap_or(source.len());
    let end_byte = source
        .char_indices()
        .nth(end)
        .map(|(b, _)| b)
        .unwrap_or(source.len());

    let mut out = String::with_capacity(end_byte.saturating_sub(start_byte) + 8);
    if start > 0 {
        out.push('…');
    }
    out.push_str(source[start_byte..end_byte].trim());
    if end < total_chars {
        out.push('…');
    }
    out
}

fn find_lower_char_pos(haystack_lower: &str, needle_lower: &str) -> Option<usize> {
    if needle_lower.is_empty() {
        return None;
    }
    let byte_idx = haystack_lower.find(needle_lower)?;
    Some(haystack_lower[..byte_idx].chars().count())
}
