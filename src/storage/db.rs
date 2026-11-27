//! SQLite schema initialization, WAL pragmas, and FTS5 setup.

use anyhow::{Context, Result};
use rusqlite::Connection;

const FIRST_PURE_TEAMS_SCHEMA_VERSION: i32 = 52;
const SCHEMA_USER_VERSION_UNICODE61: i32 = 58;
const SCHEMA_USER_VERSION_TRIGRAM: i32 = 59;

/// Normalizes an RFC3339 timestamp to a fixed-width UTC comparison key with 9 fractional digits
/// (`YYYY-MM-DDTHH:MM:SS.NNNNNNNNNZ`). Empty or non-RFC3339 values are returned trimmed as-is.
pub fn normalize_timestamp_sort_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    match chrono::DateTime::parse_from_rfc3339(trimmed) {
        Ok(dt) => dt
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        Err(_) => trimmed.to_string(),
    }
}

/// Initializes SQLite PRAGMAs, tables, indexes, and FTS5 virtual table.
/// Returns `true` if `messages_fts` uses the `trigram` tokenizer, `false` if using `unicode61`.
pub fn init_connection(conn: &Connection, is_memory: bool) -> Result<bool> {
    conn.set_prepared_statement_cache_capacity(64);
    if !is_memory {
        let _ = conn.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA journal_size_limit = 67108864;
            ",
        );
    }
    conn.execute_batch(
        "
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -8000;
        ",
    )
    .context("Failed to configure SQLite PRAGMAs")?;
    let mut prev_user_version: i32 = 0;
    if !is_memory {
        let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
        let user_version: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap_or(0);
        prev_user_version = user_version;
        if user_version == SCHEMA_USER_VERSION_TRIGRAM
            || user_version == SCHEMA_USER_VERSION_UNICODE61
        {
            let intact: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages_fts')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='thread_reply_windows')
                     AND EXISTS(SELECT 1 FROM pragma_table_info('messages') WHERE name='created_at_sort')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_messages_conv_root_created_id')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_messages_has_mentions')
                     AND EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='idx_conversations_sort_v2')",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if intact {
                return Ok(user_version == SCHEMA_USER_VERSION_TRIGRAM);
            }
        }
        if user_version < FIRST_PURE_TEAMS_SCHEMA_VERSION {
            conn.execute_batch(
                "
                DROP TABLE IF EXISTS mail_fts;
                DROP TABLE IF EXISTS mail_messages;
                DROP TABLE IF EXISTS mail_folders;
                DROP TABLE IF EXISTS calendar_events;
                DROP TABLE IF EXISTS contacts;
                DROP TABLE IF EXISTS todo_tasks;
                DROP TABLE IF EXISTS todo_lists;
                DROP TABLE IF EXISTS onenote_fts;
                DROP TABLE IF EXISTS onenote_page_details;
                DROP TABLE IF EXISTS onenote_pages;
                DROP TABLE IF EXISTS onenote_sections;
                DROP TABLE IF EXISTS onenote_section_groups;
                DROP TABLE IF EXISTS onenote_notebooks;
                ",
            )
            .context("Failed to drop legacy Outlook/OneNote tables")?;
        }
    }

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS conversations (
            conv_key TEXT PRIMARY KEY,
            target_json TEXT NOT NULL,
            kind TEXT NOT NULL,
            team_id TEXT,
            team_name TEXT,
            title TEXT NOT NULL,
            topic TEXT,
            unread_count INTEGER NOT NULL DEFAULT 0,
            last_read_at TEXT,
            last_msg_id TEXT,
            last_msg_author TEXT,
            last_msg_preview TEXT,
            last_updated_at TEXT,
            pinned INTEGER NOT NULL DEFAULT 0,
            peer_user_id TEXT,
            web_url TEXT
        );

        DROP INDEX IF EXISTS idx_conversations_sort;

        CREATE INDEX IF NOT EXISTS idx_conversations_sort_v2
            ON conversations(pinned DESC, COALESCE(last_updated_at, '') DESC, title ASC);

        CREATE TABLE IF NOT EXISTS teams (
            team_id TEXT PRIMARY KEY,
            display_name TEXT NOT NULL,
            description TEXT,
            channels_json TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            msg_key TEXT PRIMARY KEY,
            conv_key TEXT NOT NULL,
            msg_id TEXT NOT NULL,
            parent_msg_id TEXT,
            reply_to_id TEXT,
            reply_to_author TEXT,
            reply_to_preview TEXT,
            subject TEXT,
            author_id TEXT,
            author_name TEXT NOT NULL,
            is_me INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            created_at_sort TEXT NOT NULL DEFAULT '',
            last_modified_at TEXT,
            deleted INTEGER NOT NULL DEFAULT 0,
            importance TEXT NOT NULL,
            message_type TEXT NOT NULL,
            body_html TEXT NOT NULL,
            body_plain TEXT NOT NULL,
            attachments_json TEXT NOT NULL,
            mentions_json TEXT NOT NULL,
            reactions_json TEXT NOT NULL,
            reply_count INTEGER NOT NULL DEFAULT 0,
            web_url TEXT,
            has_mentions INTEGER NOT NULL DEFAULT 0
        );

        DROP INDEX IF EXISTS idx_messages_conv_parent_created;

        CREATE INDEX IF NOT EXISTS idx_messages_conv_msg
            ON messages(conv_key, msg_id);

        CREATE TABLE IF NOT EXISTS members (
            conv_key TEXT NOT NULL,
            user_id TEXT NOT NULL,
            member_id TEXT,
            display_name TEXT NOT NULL,
            email TEXT,
            roles_json TEXT NOT NULL,
            PRIMARY KEY (conv_key, user_id)
        );

        CREATE TABLE IF NOT EXISTS presences (
            user_id TEXT PRIMARY KEY,
            availability TEXT NOT NULL,
            activity TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sync_cursors (
            scope_key TEXT PRIMARY KEY,
            delta_link TEXT,
            oldest_msg_ts TEXT,
            synced_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS thread_reply_windows (
            conv_key TEXT NOT NULL,
            root_msg_id TEXT NOT NULL,
            reply_ids_json TEXT NOT NULL,
            complete INTEGER NOT NULL,
            PRIMARY KEY (conv_key, root_msg_id)
        );
        ",
    )
    .context("Failed to create base SQLite schema")?;

    let has_mentions_col: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('messages') WHERE name = 'has_mentions')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_mentions_col {
        conn.execute_batch(
            "
            ALTER TABLE messages ADD COLUMN has_mentions INTEGER NOT NULL DEFAULT 0;
            UPDATE messages SET has_mentions = 1 WHERE mentions_json != '[]' AND mentions_json != '';
            ",
        )
        .context("Failed to migrate messages.has_mentions column")?;
    }

    let has_created_at_sort_col: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('messages') WHERE name = 'created_at_sort')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    let root_idx_uses_sort: bool = conn
        .query_row(
            "SELECT COALESCE(sql, '') FROM sqlite_master WHERE type = 'index' AND name = 'idx_messages_conv_root_created_id'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(|sql| sql.contains("created_at_sort"))
        .unwrap_or(false);
    let needs_backfill = if has_created_at_sort_col {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE created_at_sort = '' AND created_at != '')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
    } else {
        true
    };

    if !has_created_at_sort_col || needs_backfill || !root_idx_uses_sort {
        let tx = conn
            .unchecked_transaction()
            .context("Failed to begin messages.created_at_sort migration transaction")?;
        if !has_created_at_sort_col {
            tx.execute_batch(
                "ALTER TABLE messages ADD COLUMN created_at_sort TEXT NOT NULL DEFAULT '';",
            )
            .context("Failed to add messages.created_at_sort column")?;
        }
        let rows_to_fill: Vec<(String, String)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT msg_key, created_at FROM messages WHERE created_at_sort = '' AND created_at != ''",
            )?;
            let mapped = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !rows_to_fill.is_empty() {
            let mut upd =
                tx.prepare_cached("UPDATE messages SET created_at_sort = ?1 WHERE msg_key = ?2")?;
            for (msg_key, created_at) in rows_to_fill {
                let sort_key = normalize_timestamp_sort_key(&created_at);
                upd.execute(rusqlite::params![sort_key, msg_key])?;
            }
        }
        if !root_idx_uses_sort {
            tx.execute_batch(
                "
                DROP INDEX IF EXISTS idx_messages_conv_root_created_id;
                DROP INDEX IF EXISTS idx_messages_created;
                DROP INDEX IF EXISTS idx_messages_has_mentions;
                ",
            )
            .context("Failed to drop legacy created_at indexes")?;
        }
        tx.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_messages_conv_root_created_id
                ON messages(conv_key, parent_msg_id, created_at_sort DESC, msg_id DESC);
            CREATE INDEX IF NOT EXISTS idx_messages_created
                ON messages(created_at_sort DESC);
            CREATE INDEX IF NOT EXISTS idx_messages_has_mentions
                ON messages(created_at_sort DESC, msg_id DESC)
                WHERE deleted = 0 AND has_mentions = 1;
            ",
        )
        .context("Failed to create created_at_sort indexes")?;
        tx.commit()
            .context("Failed to commit messages.created_at_sort migration transaction")?;
    } else {
        conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_messages_conv_root_created_id
                ON messages(conv_key, parent_msg_id, created_at_sort DESC, msg_id DESC);
            CREATE INDEX IF NOT EXISTS idx_messages_created
                ON messages(created_at_sort DESC);
            CREATE INDEX IF NOT EXISTS idx_messages_has_mentions
                ON messages(created_at_sort DESC, msg_id DESC)
                WHERE deleted = 0 AND has_mentions = 1;
            ",
        )
        .context("Failed to ensure created_at_sort indexes")?;
    }

    let messages_trigram = ensure_fts5_table(
        conn,
        "messages_fts",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            msg_key UNINDEXED,
            conv_key UNINDEXED,
            author_name,
            body_plain,
            subject,
            tokenize='trigram'
        );
        ",
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
            msg_key UNINDEXED,
            conv_key UNINDEXED,
            author_name,
            body_plain,
            subject,
            tokenize='unicode61'
        );
        ",
    )?;

    if !is_memory && prev_user_version < SCHEMA_USER_VERSION_UNICODE61 {
        let tx = conn
            .unchecked_transaction()
            .context("Failed to begin messages_fts rowid alignment migration")?;
        tx.execute_batch(
            "
            DELETE FROM messages_fts;
            INSERT INTO messages_fts (rowid, msg_key, conv_key, author_name, body_plain, subject)
            SELECT rowid, msg_key, conv_key, author_name, body_plain, COALESCE(subject, '')
            FROM messages
            WHERE deleted = 0;
            ",
        )
        .context("Failed to align messages_fts rowids with messages.rowid")?;
        tx.commit()
            .context("Failed to commit messages_fts rowid alignment migration")?;
    }

    if !is_memory {
        let version = if messages_trigram {
            SCHEMA_USER_VERSION_TRIGRAM
        } else {
            SCHEMA_USER_VERSION_UNICODE61
        };
        conn.execute_batch(&format!("PRAGMA user_version = {version};"))
            .context("Failed to set SQLite user_version")?;
    }

    Ok(messages_trigram)
}

/// Configures a read-only WAL reader connection for concurrent UI reads.
pub fn init_reader_connection(conn: &Connection) -> Result<()> {
    conn.set_prepared_statement_cache_capacity(64);
    conn.execute_batch(
        "
        PRAGMA query_only = ON;
        PRAGMA busy_timeout = 5000;
        PRAGMA temp_store = MEMORY;
        PRAGMA cache_size = -4000;
        ",
    )
    .context("Failed to configure SQLite reader PRAGMAs")?;
    let _ = conn.execute_batch("PRAGMA mmap_size = 67108864;");
    Ok(())
}

/// Releases unused SQLite page-cache memory back to the OS (`PRAGMA shrink_memory`).
pub fn shrink_connection_memory(conn: &Connection) {
    let _ = conn.execute_batch("PRAGMA shrink_memory;");
}

fn ensure_fts5_table(
    conn: &Connection,
    table_name: &str,
    trigram_sql: &str,
    unicode61_sql: &str,
) -> Result<bool> {
    if conn.execute_batch(trigram_sql).is_err() {
        conn.execute_batch(unicode61_sql)
            .with_context(|| format!("Failed to create FTS5 virtual table ({table_name}, unicode61)"))?;
        return Ok(false);
    }

    let sql_def: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get(0),
        )
        .ok();
    let uses_trigram = sql_def
        .map(|s| s.to_ascii_lowercase().contains("trigram"))
        .unwrap_or(true);
    Ok(uses_trigram)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const LEGACY_TABLES: &[&str] = &[
        "mail_fts",
        "mail_messages",
        "mail_folders",
        "calendar_events",
        "contacts",
        "todo_tasks",
        "todo_lists",
        "onenote_fts",
        "onenote_page_details",
        "onenote_pages",
        "onenote_sections",
        "onenote_section_groups",
        "onenote_notebooks",
    ];

    fn table_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name=?1)",
            [name],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn test_pure_teams_schema_and_user_version_fast_path() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("fastpath.db");

        // First open initializes pure-Teams schema and stamps user_version
        let first_trigram = {
            let conn = Connection::open(&db_path).unwrap();
            let trigram = init_connection(&conn, false).unwrap();
            let ver: i32 = conn
                .query_row("PRAGMA user_version;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                ver,
                if trigram {
                    SCHEMA_USER_VERSION_TRIGRAM
                } else {
                    SCHEMA_USER_VERSION_UNICODE61
                }
            );
            let cache_size: i32 = conn
                .query_row("PRAGMA cache_size;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(cache_size, -8000);

            for required in [
                "conversations",
                "teams",
                "messages",
                "members",
                "presences",
                "sync_cursors",
                "thread_reply_windows",
                "messages_fts",
            ] {
                assert!(table_exists(&conn, required), "{required} should exist");
            }
            assert!(index_exists(&conn, "idx_messages_conv_root_created_id"));
            for legacy in LEGACY_TABLES {
                assert!(
                    !table_exists(&conn, legacy),
                    "legacy table {legacy} must not exist"
                );
            }
            trigram
        };

        // Second open takes user_version fast-path and preserves trigram_enabled
        {
            let conn = Connection::open(&db_path).unwrap();
            let second_trigram = init_connection(&conn, false).unwrap();
            assert_eq!(second_trigram, first_trigram);

            // Force unicode61 user_version stamp and verify fast-path returns false without running DDL
            conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_USER_VERSION_UNICODE61};"))
                .unwrap();
        }
        {
            let conn = Connection::open(&db_path).unwrap();
            assert!(!init_connection(&conn, false).unwrap());
        }

        // Reader connection uses -4000 cache cap
        let reader = Connection::open_with_flags(
            &db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        init_reader_connection(&reader).unwrap();
        let reader_cache: i32 = reader
            .query_row("PRAGMA cache_size;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(reader_cache, -4000);
    }

    #[test]
    fn test_legacy_outlook_onenote_tables_dropped_on_upgrade() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("legacy_v41.db");

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE conversations (
                    conv_key TEXT PRIMARY KEY,
                    target_json TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    team_id TEXT,
                    team_name TEXT,
                    title TEXT NOT NULL,
                    topic TEXT,
                    unread_count INTEGER NOT NULL DEFAULT 0,
                    last_read_at TEXT,
                    last_msg_id TEXT,
                    last_msg_author TEXT,
                    last_msg_preview TEXT,
                    last_updated_at TEXT,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    peer_user_id TEXT,
                    web_url TEXT
                );
                INSERT INTO conversations (conv_key, target_json, kind, title)
                VALUES ('chat:c1', '{\"target_type\":\"chat\",\"chat_id\":\"c1\"}', 'oneOnOne', 'Alice');

                CREATE TABLE mail_folders (id TEXT PRIMARY KEY);
                CREATE TABLE mail_messages (id TEXT PRIMARY KEY);
                CREATE VIRTUAL TABLE mail_fts USING fts5(msg_id UNINDEXED, body_plain);
                CREATE TABLE calendar_events (id TEXT PRIMARY KEY);
                CREATE TABLE contacts (id TEXT PRIMARY KEY);
                CREATE TABLE todo_lists (id TEXT PRIMARY KEY);
                CREATE TABLE todo_tasks (id TEXT PRIMARY KEY);
                CREATE TABLE onenote_notebooks (notebook_key TEXT PRIMARY KEY);
                CREATE TABLE onenote_section_groups (section_group_key TEXT PRIMARY KEY);
                CREATE TABLE onenote_sections (section_key TEXT PRIMARY KEY);
                CREATE TABLE onenote_pages (page_key TEXT PRIMARY KEY);
                CREATE TABLE onenote_page_details (page_key TEXT PRIMARY KEY);
                CREATE VIRTUAL TABLE onenote_fts USING fts5(page_key UNINDEXED, body_plain);
                PRAGMA user_version = 41;
                ",
            )
            .unwrap();
            for legacy in LEGACY_TABLES {
                assert!(table_exists(&conn, legacy), "setup expected {legacy}");
            }
        }

        // Upgrade via init_connection drops all legacy tables while keeping Teams data intact
        {
            let conn = Connection::open(&db_path).unwrap();
            let trigram = init_connection(&conn, false).unwrap();
            let ver: i32 = conn
                .query_row("PRAGMA user_version;", [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                ver,
                if trigram {
                    SCHEMA_USER_VERSION_TRIGRAM
                } else {
                    SCHEMA_USER_VERSION_UNICODE61
                }
            );
            for legacy in LEGACY_TABLES {
                assert!(
                    !table_exists(&conn, legacy),
                    "legacy table {legacy} should have been dropped on upgrade"
                );
            }
            assert!(index_exists(&conn, "idx_messages_conv_root_created_id"));
            let title: String = conn
                .query_row(
                    "SELECT title FROM conversations WHERE conv_key = 'chat:c1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(title, "Alice");
        }
    }

    #[test]
    fn test_v51_migration_adds_has_mentions_and_aligns_indexes() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("v51_upgrade.db");

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE conversations (
                    conv_key TEXT PRIMARY KEY,
                    target_json TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    team_id TEXT,
                    team_name TEXT,
                    title TEXT NOT NULL,
                    topic TEXT,
                    unread_count INTEGER NOT NULL DEFAULT 0,
                    last_read_at TEXT,
                    last_msg_id TEXT,
                    last_msg_author TEXT,
                    last_msg_preview TEXT,
                    last_updated_at TEXT,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    peer_user_id TEXT,
                    web_url TEXT
                );
                CREATE INDEX idx_conversations_sort ON conversations(pinned DESC, last_updated_at DESC);
                CREATE TABLE messages (
                    msg_key TEXT PRIMARY KEY,
                    conv_key TEXT NOT NULL,
                    msg_id TEXT NOT NULL,
                    parent_msg_id TEXT,
                    reply_to_id TEXT,
                    reply_to_author TEXT,
                    reply_to_preview TEXT,
                    subject TEXT,
                    author_id TEXT,
                    author_name TEXT NOT NULL,
                    is_me INTEGER NOT NULL DEFAULT 0,
                    created_at TEXT NOT NULL,
                    last_modified_at TEXT,
                    deleted INTEGER NOT NULL DEFAULT 0,
                    importance TEXT NOT NULL,
                    message_type TEXT NOT NULL,
                    body_html TEXT NOT NULL,
                    body_plain TEXT NOT NULL,
                    attachments_json TEXT NOT NULL,
                    mentions_json TEXT NOT NULL,
                    reactions_json TEXT NOT NULL,
                    reply_count INTEGER NOT NULL DEFAULT 0,
                    web_url TEXT
                );
                CREATE INDEX idx_messages_conv_parent_created ON messages(conv_key, parent_msg_id, created_at);
                INSERT INTO messages (
                    msg_key, conv_key, msg_id, author_name, created_at, importance,
                    message_type, body_html, body_plain, attachments_json, mentions_json, reactions_json
                ) VALUES
                    ('chat:c1|m1', 'chat:c1', 'm1', 'Alice', '2026-11-15T10:00:00Z', 'normal', 'message', '<p>hi</p>', 'hi', '[]', '[{\"id\":0,\"user_id\":\"u1\",\"display_name\":\"Bob\"}]', '[]'),
                    ('chat:c1|m2', 'chat:c1', 'm2', 'Bob', '2026-11-15T10:01:00Z', 'normal', 'message', '<p>yo</p>', 'yo', '[]', '[]', '[]');
                PRAGMA user_version = 51;
                ",
            )
            .unwrap();
        }

        let conn = Connection::open(&db_path).unwrap();
        let trigram = init_connection(&conn, false).unwrap();
        let ver: i32 = conn
            .query_row("PRAGMA user_version;", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            ver,
            if trigram {
                SCHEMA_USER_VERSION_TRIGRAM
            } else {
                SCHEMA_USER_VERSION_UNICODE61
            }
        );

        assert!(index_exists(&conn, "idx_messages_has_mentions"));
        assert!(index_exists(&conn, "idx_conversations_sort_v2"));
        assert!(!index_exists(&conn, "idx_conversations_sort"));
        assert!(!index_exists(&conn, "idx_messages_conv_parent_created"));

        let hm1: i32 = conn
            .query_row("SELECT has_mentions FROM messages WHERE msg_key = 'chat:c1|m1'", [], |r| r.get(0))
            .unwrap();
        let hm2: i32 = conn
            .query_row("SELECT has_mentions FROM messages WHERE msg_key = 'chat:c1|m2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hm1, 1);
        assert_eq!(hm2, 0);
    }

    #[test]
    fn test_v57_migration_aligns_fts5_rowids() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("v57_fts_rowid.db");

        {
            let conn = Connection::open(&db_path).unwrap();
            let _ = init_connection(&conn, false).unwrap();
            // Simulate a v57 database where messages.rowid and messages_fts.rowid diverged.
            conn.execute_batch(
                "
                INSERT INTO messages (
                    rowid, msg_key, conv_key, msg_id, author_name, created_at, created_at_sort,
                    importance, message_type, body_html, body_plain, attachments_json, mentions_json, reactions_json
                ) VALUES
                    (42, 'chat:c1|m1', 'chat:c1', 'm1', 'Alice', '2026-11-15T10:00:00Z', '2026-11-15T10:00:00.000000000Z',
                     'normal', 'message', '<p>Quantum deployment</p>', 'Quantum deployment', '[]', '[]', '[]');
                DELETE FROM messages_fts;
                INSERT INTO messages_fts (rowid, msg_key, conv_key, author_name, body_plain, subject)
                VALUES (1, 'chat:c1|m1', 'chat:c1', 'Alice', 'Quantum deployment', '');
                PRAGMA user_version = 57;
                ",
            )
            .unwrap();
        }

        // Re-opening upgrades v57 -> v58/v59 and aligns messages_fts.rowid = messages.rowid (42).
        let conn = Connection::open(&db_path).unwrap();
        let _ = init_connection(&conn, false).unwrap();
        let fts_rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM messages_fts WHERE msg_key = 'chat:c1|m1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts_rowid, 42);
    }
}
