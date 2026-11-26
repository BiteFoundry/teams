//! Local SQLite WAL persistence + FTS5 (`trigram` + `LIKE` hybrid) search repository.

pub mod db;
pub mod repo;
pub mod search;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::Result;

use crate::models::{
    ConversationSummary, JoinedTeams, MemberInfo, MessageItem, PresenceStatus, ReactionSummary,
    SearchHit, SyncCursor, TeamSummary, ThreadCoverage, ThreadView,
};

pub use repo::UpsertMessagesOutcome;

const MAX_READER_POOL_SIZE: usize = 4;

#[derive(Clone)]
pub struct Storage {
    path: PathBuf,
    conn: Arc<Mutex<rusqlite::Connection>>,
    readers: Option<Arc<Mutex<Vec<rusqlite::Connection>>>>,
    trigram_enabled: bool,
}

enum ReadConnInner<'a> {
    Pooled {
        conn: Option<rusqlite::Connection>,
        pool: &'a Arc<Mutex<Vec<rusqlite::Connection>>>,
    },
    Writer(std::sync::MutexGuard<'a, rusqlite::Connection>),
}

struct ReadConnGuard<'a>(ReadConnInner<'a>);

impl std::ops::Deref for ReadConnGuard<'_> {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        match &self.0 {
            ReadConnInner::Pooled { conn, .. } => conn
                .as_ref()
                .expect("pooled reader connection available until Drop"),
            ReadConnInner::Writer(guard) => guard,
        }
    }
}

impl Drop for ReadConnGuard<'_> {
    fn drop(&mut self) {
        if let ReadConnInner::Pooled { conn, pool } = &mut self.0 {
            if let Some(c) = conn.take() {
                if let Ok(mut guard) = pool.lock() {
                    if guard.len() < MAX_READER_POOL_SIZE {
                        guard.push(c);
                    }
                }
            }
        }
    }
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        crate::config::ensure_parent(path)?;
        let conn = rusqlite::Connection::open(path)?;
        let trigram_enabled = db::init_connection(&conn, false)?;
        Ok(Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
            readers: Some(Arc::new(Mutex::new(Vec::with_capacity(
                MAX_READER_POOL_SIZE,
            )))),
            trigram_enabled,
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = rusqlite::Connection::open_in_memory()?;
        let trigram_enabled = db::init_connection(&conn, true)?;
        Ok(Self {
            path: PathBuf::from(":memory:"),
            conn: Arc::new(Mutex::new(conn)),
            readers: None,
            trigram_enabled,
        })
    }

    /// Opens an existing database with a single read-only connection without creating an empty DB file.
    pub fn open_readonly(path: &Path) -> Result<Self> {
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = rusqlite::Connection::open_with_flags(path, flags)?;
        db::init_reader_connection(&conn)?;
        let trigram_enabled: bool = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'messages_fts'",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|s| s.to_ascii_lowercase().contains("trigram"))
            .unwrap_or(false);
        Ok(Self {
            path: path.to_path_buf(),
            conn: Arc::new(Mutex::new(conn)),
            readers: None,
            trigram_enabled,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn conn(&self) -> &Arc<Mutex<rusqlite::Connection>> {
        &self.conn
    }

    pub fn trigram_enabled(&self) -> bool {
        self.trigram_enabled
    }

    /// Releases unused SQLite page-cache memory back to the OS (`PRAGMA shrink_memory`) and runs `PRAGMA optimize`.
    pub fn shrink_memory(&self) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute_batch("PRAGMA optimize;");
            db::shrink_connection_memory(&conn);
        }
        if let Some(pool) = &self.readers {
            if let Ok(guard) = pool.lock() {
                for c in guard.iter() {
                    db::shrink_connection_memory(c);
                }
            }
        }
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("Failed to acquire SQLite connection lock (poisoned mutex)"))
    }

    fn read_conn(&self) -> Result<ReadConnGuard<'_>> {
        if let Some(pool) = &self.readers {
            let existing = {
                let mut guard = pool.lock().map_err(|_| {
                    anyhow::anyhow!("Failed to acquire SQLite reader pool lock (poisoned mutex)")
                })?;
                guard.pop()
            };
            let conn = match existing {
                Some(c) => c,
                None => {
                    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
                    let c = rusqlite::Connection::open_with_flags(&self.path, flags)?;
                    db::init_reader_connection(&c)?;
                    c
                }
            };
            Ok(ReadConnGuard(ReadConnInner::Pooled {
                conn: Some(conn),
                pool,
            }))
        } else {
            Ok(ReadConnGuard(ReadConnInner::Writer(self.lock_conn()?)))
        }
    }

    // --- Conversations & Teams ---
    pub fn upsert_conversations(&self, items: &[ConversationSummary]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_conversations(&mut conn, items)
    }

    pub(crate) fn upsert_conversations_with_null_topics(
        &self,
        items: &[ConversationSummary],
        clear_null_topic_keys: &HashSet<String>,
    ) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_conversations_with_null_topics(&mut conn, items, clear_null_topic_keys)
    }

    pub(crate) fn save_updated_chat_topic(
        &self,
        chat: &ConversationSummary,
    ) -> Result<ConversationSummary> {
        let mut conn = self.lock_conn()?;
        repo::save_updated_chat_topic(&mut conn, chat)
    }

    pub fn upsert_teams(&self, teams: &[TeamSummary]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_teams(&mut conn, teams)
    }

    /// Stores fetched teams; a team whose channel fetch failed keeps its cached channels.
    /// Teams and channels Graph no longer lists are removed from the cache (their message
    /// rows stay); an empty fetch removes nothing.
    pub fn upsert_joined_teams(&self, fetched: &JoinedTeams) -> Result<Vec<TeamSummary>> {
        let cached = self.list_teams()?;
        let mut merged = fetched.teams.clone();
        for failed in &fetched.channel_errors {
            if let (Some(team), Some(old)) = (
                merged.iter_mut().find(|t| t.id == failed.team_id),
                cached.iter().find(|t| t.id == failed.team_id),
            ) {
                team.channels = old.channels.clone();
            }
        }
        let failed: std::collections::HashSet<String> =
            fetched.channel_errors.iter().map(|e| e.team_id.clone()).collect();
        let mut conn = self.lock_conn()?;
        repo::upsert_joined_teams_and_prune(&mut conn, &merged, &failed)?;
        Ok(merged)
    }

    pub fn list_conversations(&self) -> Result<Vec<ConversationSummary>> {
        let conn = self.read_conn()?;
        repo::list_conversations(&conn)
    }

    pub fn list_teams(&self) -> Result<Vec<TeamSummary>> {
        let conn = self.read_conn()?;
        repo::list_teams(&conn)
    }

    pub fn get_conversation(&self, conv_key: &str) -> Result<Option<ConversationSummary>> {
        let conn = self.read_conn()?;
        repo::get_conversation(&conn, conv_key)
    }

    pub fn mark_conversation_read(&self, conv_key: &str, read_at: &str) -> Result<()> {
        let conn = self.lock_conn()?;
        repo::mark_conversation_read(&conn, conv_key, read_at)
    }

    pub fn increment_conversation_unread(&self, conv_key: &str, delta: i32) -> Result<()> {
        let conn = self.lock_conn()?;
        repo::increment_conversation_unread(&conn, conv_key, delta)
    }

    pub fn set_conversation_pinned(&self, conv_key: &str, pinned: bool) -> Result<()> {
        let conn = self.lock_conn()?;
        repo::set_conversation_pinned(&conn, conv_key, pinned)
    }

    // --- Messages & Threads ---
    pub fn upsert_messages(&self, messages: &[MessageItem]) -> Result<usize> {
        let mut conn = self.lock_conn()?;
        repo::upsert_messages(&mut conn, messages)
    }

    pub fn upsert_messages_returning_changed_ids(
        &self,
        messages: &[MessageItem],
    ) -> Result<Vec<String>> {
        let mut conn = self.lock_conn()?;
        repo::upsert_messages_returning_changed_ids(&mut conn, messages)
    }

    pub fn upsert_messages_detailed(
        &self,
        messages: &[MessageItem],
    ) -> Result<repo::UpsertMessagesOutcome> {
        let mut conn = self.lock_conn()?;
        repo::upsert_messages_detailed(&mut conn, messages)
    }

    /// Applies an accepted remote edit to a cached message via the standard upsert path
    /// (updating `messages`, `messages_fts`, and `conversations.last_msg_preview`).
    pub fn apply_local_edit(
        &self,
        conv_key: &str,
        msg_id: &str,
        new_body_html: &str,
        edited_at: &str,
    ) -> Result<Option<(MessageItem, repo::UpsertMessagesOutcome)>> {
        let mut conn = self.lock_conn()?;
        let Some(mut msg) = repo::get_message(&conn, conv_key, msg_id)? else {
            return Ok(None);
        };
        msg.body_html = new_body_html.to_string();
        msg.body_plain = crate::parser::html_and_cards_to_plain(new_body_html, &msg.attachments);
        msg.mentions
            .retain(|m| new_body_html.contains(&format!("<at id=\"{}\"", m.id)));
        msg.last_modified_at = Some(edited_at.to_string());
        msg.deleted = false;
        let outcome = repo::upsert_messages_detailed(&mut conn, std::slice::from_ref(&msg))?;
        Ok(Some((msg, outcome)))
    }

    /// Applies an accepted remote soft-delete to a cached message via the standard upsert path
    /// (removing from `messages_fts`, recalculating `reply_count`, and updating `last_msg_preview`).
    pub fn apply_local_soft_delete(
        &self,
        conv_key: &str,
        msg_id: &str,
        deleted_at: &str,
    ) -> Result<Option<(MessageItem, repo::UpsertMessagesOutcome)>> {
        let mut conn = self.lock_conn()?;
        let Some(mut msg) = repo::get_message(&conn, conv_key, msg_id)? else {
            return Ok(None);
        };
        msg.deleted = true;
        msg.subject = None;
        msg.body_html.clear();
        msg.body_plain = "(deleted message)".to_string();
        msg.attachments.clear();
        msg.mentions.clear();
        msg.reactions.clear();
        msg.reply_to_author = None;
        msg.reply_to_preview = None;
        msg.last_modified_at = Some(deleted_at.to_string());
        let outcome = repo::upsert_messages_detailed(&mut conn, std::slice::from_ref(&msg))?;
        Ok(Some((msg, outcome)))
    }

    /// Applies an accepted remote reaction toggle to a cached message via the standard upsert path.
    pub fn apply_local_reaction(
        &self,
        conv_key: &str,
        msg_id: &str,
        reaction_type: &str,
        enable: bool,
        my_display_name: Option<&str>,
    ) -> Result<Option<(MessageItem, repo::UpsertMessagesOutcome)>> {
        let mut conn = self.lock_conn()?;
        let Some(mut msg) = repo::get_message(&conn, conv_key, msg_id)? else {
            return Ok(None);
        };
        apply_reaction_toggle(&mut msg, reaction_type, enable, my_display_name);
        let outcome = repo::upsert_messages_detailed(&mut conn, std::slice::from_ref(&msg))?;
        Ok(Some((msg, outcome)))
    }

    /// Lists root messages in chronological order (oldest first, newest last), limited to newest `limit`.
    pub fn list_root_messages(
        &self,
        conv_key: &str,
        limit: usize,
        before_created_at: Option<&str>,
    ) -> Result<Vec<MessageItem>> {
        let conn = self.read_conn()?;
        repo::list_root_messages(&conn, conv_key, limit, before_created_at)
    }

    pub fn list_thread_replies(
        &self,
        conv_key: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        let conn = self.read_conn()?;
        repo::list_thread_replies(&conn, conv_key, root_msg_id, limit)
    }

    pub fn get_message(&self, conv_key: &str, msg_id: &str) -> Result<Option<MessageItem>> {
        let conn = self.read_conn()?;
        repo::get_message(&conn, conv_key, msg_id)
    }

    pub fn get_messages_by_ids(
        &self,
        conv_key: &str,
        msg_ids: &[String],
    ) -> Result<Vec<MessageItem>> {
        if msg_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.read_conn()?;
        repo::get_messages_by_ids(&conn, conv_key, msg_ids)
    }

    pub fn get_thread(
        &self,
        conv_key: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<Option<ThreadView>> {
        let conn = self.read_conn()?;
        repo::get_thread(&conn, conv_key, root_msg_id, limit)
    }

    pub(crate) fn save_fetched_thread(&self, thread: &ThreadView, complete: bool) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::save_fetched_thread(&mut conn, thread, complete)
    }

    pub(crate) fn get_fetched_thread(
        &self,
        conv_key: &str,
        root_msg_id: &str,
        limit: usize,
    ) -> Result<Option<(ThreadView, ThreadCoverage)>> {
        let conn = self.read_conn()?;
        repo::get_fetched_thread(&conn, conv_key, root_msg_id, limit)
    }

    /// Lists messages across all conversations where `@me` is mentioned.
    pub fn list_mentions_for_user(
        &self,
        my_user_id: &str,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        let conn = self.read_conn()?;
        repo::list_mentions_for_user(&conn, my_user_id, limit)
    }

    // --- Members & Presence ---
    pub fn upsert_members(&self, conv_key: &str, members: &[MemberInfo]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_members(&mut conn, conv_key, members)
    }

    pub fn replace_members(&self, conv_key: &str, members: &[MemberInfo]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::replace_members(&mut conn, conv_key, members)
    }

    pub fn list_members(&self, conv_key: &str) -> Result<Vec<MemberInfo>> {
        let conn = self.read_conn()?;
        repo::list_members(&conn, conv_key)
    }

    pub fn upsert_presences(&self, presences: &[PresenceStatus]) -> Result<()> {
        let mut conn = self.lock_conn()?;
        repo::upsert_presences(&mut conn, presences)
    }

    pub fn get_presences(&self) -> Result<HashMap<String, PresenceStatus>> {
        let conn = self.read_conn()?;
        repo::get_presences(&conn)
    }

    // --- Search & Sync Cursors ---
    pub fn search_messages(
        &self,
        query: &str,
        conv_key_filter: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let conn = self.read_conn()?;
        search::search_messages(&conn, query, conv_key_filter, limit)
    }

    /// Lists root messages from `(from_created_at, from_msg_id)` on (inclusive), oldest first.
    pub fn list_root_messages_from(
        &self,
        conv_key: &str,
        from_created_at: &str,
        from_msg_id: &str,
        limit: usize,
    ) -> Result<Vec<MessageItem>> {
        let conn = self.read_conn()?;
        repo::list_root_messages_from(&conn, conv_key, from_created_at, from_msg_id, limit)
    }

    /// Lists older root messages strictly preceding `(before_created_at, before_msg_id)` in chronological order (oldest first).
    pub fn list_older_root_messages(
        &self,
        conv_key: &str,
        limit: usize,
        before_created_at: &str,
        before_msg_id: Option<&str>,
    ) -> Result<Vec<MessageItem>> {
        let conn = self.read_conn()?;
        repo::list_older_root_messages(&conn, conv_key, limit, before_created_at, before_msg_id)
    }

    /// Saves a history cursor unless the stored one already reaches at least as far back.
    /// Returns whether the cursor was written.
    pub fn save_history_cursor_if_deeper(&self, cursor: &SyncCursor) -> Result<bool> {
        let mut conn = self.lock_conn()?;
        repo::save_history_cursor_if_deeper(&mut conn, cursor)
    }

    pub fn get_sync_cursor(&self, scope_key: &str) -> Result<Option<SyncCursor>> {
        let conn = self.read_conn()?;
        repo::get_sync_cursor(&conn, scope_key)
    }

    pub fn save_sync_cursor(&self, cursor: &SyncCursor) -> Result<()> {
        let conn = self.lock_conn()?;
        repo::save_sync_cursor(&conn, cursor)
    }

    /// Drops a cursor whose page link Graph no longer accepts. `save_sync_cursor` keeps the
    /// stored `oldest_msg_ts` when given none, so clearing only the link could bring back an
    /// old EOF; deleting the row returns the scope to "no cursor". A cursor whose link was
    /// replaced in the meantime is left alone.
    pub fn delete_sync_cursor_with_link(&self, scope_key: &str, delta_link: &str) -> Result<bool> {
        let conn = self.lock_conn()?;
        repo::delete_sync_cursor_with_link(&conn, scope_key, delta_link)
    }

    pub(crate) fn apply_channel_delta(
        &self,
        expected: Option<&SyncCursor>,
        cursor: &SyncCursor,
        messages: &[MessageItem],
        eligible_unread: &HashSet<&str>,
    ) -> Result<Option<UpsertMessagesOutcome>> {
        let mut conn = self.lock_conn()?;
        repo::apply_channel_delta(&mut conn, expected, cursor, messages, eligible_unread)
    }
}

pub(crate) fn apply_reaction_toggle(
    msg: &mut MessageItem,
    reaction_type: &str,
    enable: bool,
    my_display_name: Option<&str>,
) {
    let clean_name = my_display_name
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(r) = msg
        .reactions
        .iter_mut()
        .find(|r| r.reaction_type == reaction_type)
    {
        if enable {
            if !r.me_reacted {
                r.me_reacted = true;
                r.count = r.count.saturating_add(1);
            }
            if let Some(name) = clean_name {
                if !r.users.iter().any(|u| u == name) {
                    r.users.push(name.to_string());
                }
            }
        } else {
            if r.me_reacted {
                r.me_reacted = false;
                r.count = r.count.saturating_sub(1);
            }
            if let Some(name) = clean_name {
                r.users.retain(|u| u != name);
            }
        }
        msg.reactions.retain(|r| r.count > 0);
    } else if enable {
        let users = clean_name.map(|s| vec![s.to_string()]).unwrap_or_default();
        msg.reactions.push(ReactionSummary {
            reaction_type: reaction_type.to_string(),
            count: 1,
            me_reacted: true,
            users,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChannelSummary, ConversationKind, ConversationTarget, MentionInfo};

    fn sample_chat(id: &str, title: &str, pinned: bool, updated: Option<&str>) -> ConversationSummary {
        let target = ConversationTarget::Chat {
            chat_id: id.to_string(),
        };
        ConversationSummary {
            conv_key: target.conv_key(),
            target,
            kind: ConversationKind::OneOnOne,
            team_id: None,
            team_name: None,
            title: title.to_string(),
            topic: None,
            unread_count: 2,
            last_read_at: None,
            last_msg_id: None,
            last_msg_author: None,
            last_msg_preview: None,
            last_updated_at: updated.map(str::to_string),
            pinned,
            peer_user_id: Some(format!("user-{id}")),
            web_url: None,
        }
    }

    fn sample_msg(
        conv_key: &str,
        msg_id: &str,
        parent_msg_id: Option<&str>,
        author: &str,
        body_plain: &str,
        subject: Option<&str>,
        created_at: &str,
    ) -> MessageItem {
        MessageItem {
            msg_key: MessageItem::make_msg_key(conv_key, msg_id),
            conv_key: conv_key.to_string(),
            msg_id: msg_id.to_string(),
            parent_msg_id: parent_msg_id.map(str::to_string),
            reply_to_id: None,
            reply_to_author: None,
            reply_to_preview: None,
            subject: subject.map(str::to_string),
            author_id: Some("u1".to_string()),
            author_name: author.to_string(),
            is_me: false,
            created_at: created_at.to_string(),
            last_modified_at: None,
            deleted: false,
            importance: "normal".to_string(),
            message_type: "message".to_string(),
            body_html: format!("<p>{body_plain}</p>"),
            body_plain: body_plain.to_string(),
            attachments: Vec::new(),
            mentions: Vec::new(),
            reactions: Vec::new(),
            reply_count: 0,
            web_url: None,
        }
    }

    #[test]
    fn test_teams_upsert_preserves_existing_channel_state() {
        let storage = Storage::open_in_memory().unwrap();
        let ch = ChannelSummary {
            id: "ch-1".to_string(),
            team_id: "team-1".to_string(),
            team_name: "Engineering".to_string(),
            display_name: "General".to_string(),
            description: Some("General channel".to_string()),
            membership_type: "standard".to_string(),
            web_url: None,
        };
        let conv_key = ch.conv_key();

        let existing = ConversationSummary {
            conv_key: conv_key.clone(),
            target: ch.target(),
            kind: ConversationKind::Channel,
            team_id: Some("team-1".to_string()),
            team_name: Some("Engineering".to_string()),
            title: "General".to_string(),
            topic: None,
            unread_count: 7,
            last_read_at: Some("2026-11-01T00:00:00Z".to_string()),
            last_msg_id: Some("m-10".to_string()),
            last_msg_author: Some("Bob".to_string()),
            last_msg_preview: Some("Deploy complete".to_string()),
            last_updated_at: Some("2026-11-02T10:00:00Z".to_string()),
            pinned: true,
            peer_user_id: None,
            web_url: None,
        };
        storage.upsert_conversations(&[existing]).unwrap();

        let team = TeamSummary {
            id: "team-1".to_string(),
            display_name: "Engineering".to_string(),
            description: Some("Team description".to_string()),
            channels: vec![ChannelSummary {
                display_name: "General-Renamed".to_string(),
                ..ch
            }],
        };
        storage.upsert_teams(&[team]).unwrap();

        let loaded = storage.get_conversation(&conv_key).unwrap().unwrap();
        assert_eq!(loaded.title, "General-Renamed");
        assert_eq!(loaded.unread_count, 7);
        assert_eq!(loaded.last_msg_id.as_deref(), Some("m-10"));
        assert!(loaded.pinned);
    }

    #[test]
    fn test_conversation_ordering_and_mark_read() {
        let storage = Storage::open_in_memory().unwrap();
        let c1 = sample_chat("c1", "Charlie", false, Some("2026-11-15T10:00:00Z"));
        let c2 = sample_chat("c2", "Alice", true, Some("2026-11-14T10:00:00Z"));
        let c3 = sample_chat("c3", "Bob", false, Some("2026-11-15T12:00:00Z"));
        storage.upsert_conversations(&[c1, c2, c3]).unwrap();

        let list = storage.list_conversations().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].title, "Alice"); // pinned first
        assert_eq!(list[1].title, "Bob"); // newer timestamp
        assert_eq!(list[2].title, "Charlie");

        storage
            .mark_conversation_read("chat:c3", "2026-11-15T12:05:00Z")
            .unwrap();
        let c3_after = storage.get_conversation("chat:c3").unwrap().unwrap();
        assert_eq!(c3_after.unread_count, 0);
        assert_eq!(
            c3_after.last_read_at.as_deref(),
            Some("2026-11-15T12:05:00Z")
        );
    }

    #[test]
    fn test_root_messages_ordering_thread_reply_count_and_last_msg() {
        let storage = Storage::open_in_memory().unwrap();
        let team = TeamSummary {
            id: "t1".to_string(),
            display_name: "Engineering".to_string(),
            description: None,
            channels: vec![ChannelSummary {
                id: "ch1".to_string(),
                team_id: "t1".to_string(),
                team_name: "Engineering".to_string(),
                display_name: "Deploy".to_string(),
                description: None,
                membership_type: "standard".to_string(),
                web_url: None,
            }],
        };
        storage.upsert_teams(&[team]).unwrap();
        let conv_key = "chan:t1:ch1";

        let r1 = sample_msg(
            conv_key,
            "m1",
            None,
            "Bob",
            "First root",
            Some("Announcement"),
            "2026-11-15T09:00:00Z",
        );
        let r2 = sample_msg(
            conv_key,
            "m2",
            None,
            "Carol",
            "Second root",
            None,
            "2026-11-15T10:00:00Z",
        );
        let r3 = sample_msg(
            conv_key,
            "m3",
            None,
            "Dave",
            "Third root",
            None,
            "2026-11-15T11:00:00Z",
        );
        let rep1 = sample_msg(
            conv_key,
            "m1-r1",
            Some("m1"),
            "Alice",
            "Reply 1",
            None,
            "2026-11-15T11:30:00Z",
        );
        let rep2 = sample_msg(
            conv_key,
            "m1-r2",
            Some("m1"),
            "Bob",
            "Reply 2",
            None,
            "2026-11-15T12:00:00Z",
        );

        storage.upsert_messages(&[r1, r2, r3, rep1, rep2]).unwrap();

        // Newest 2 root messages, returned oldest-first (m2, m3)
        let roots = storage.list_root_messages(conv_key, 2, None).unwrap();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].msg_id, "m2");
        assert_eq!(roots[1].msg_id, "m3");

        // Pagination before m3 returns m1, m2
        let older = storage
            .list_root_messages(conv_key, 10, Some("2026-11-15T11:00:00Z"))
            .unwrap();
        assert_eq!(older.len(), 2);
        assert_eq!(older[0].msg_id, "m1");
        assert_eq!(older[0].reply_count, 2);
        assert_eq!(older[1].msg_id, "m2");

        // Thread view
        let thread = storage.get_thread(conv_key, "m1", 20).unwrap().unwrap();
        assert_eq!(thread.root.msg_id, "m1");
        assert_eq!(thread.root.reply_count, 2);
        assert_eq!(thread.replies.len(), 2);
        assert_eq!(thread.replies[0].msg_id, "m1-r1");
        assert_eq!(thread.replies[1].msg_id, "m1-r2");

        assert!(storage.get_fetched_thread(conv_key, "m1", 1).unwrap().is_none());
        let mut window = thread.clone();
        window.replies.remove(0);
        storage.save_fetched_thread(&window, false).unwrap();
        assert!(storage.get_fetched_thread(conv_key, "m1", 2).unwrap().is_none());
        assert_eq!(
            storage
                .get_fetched_thread(conv_key, "m1", 1)
                .unwrap()
                .map(|(thread, _)| thread)
                .unwrap()
                .replies,
            window.replies
        );
        storage.save_fetched_thread(&thread, true).unwrap();
        assert_eq!(
            storage
                .get_fetched_thread(conv_key, "m1", 50)
                .unwrap()
                .map(|(thread, _)| thread)
                .unwrap(),
            thread
        );
        window.replies.clear();
        storage.save_fetched_thread(&window, true).unwrap();
        let verified_empty = storage
            .get_fetched_thread(conv_key, "m1", 50)
            .unwrap()
            .map(|(thread, _)| thread)
            .unwrap();
        assert!(verified_empty.replies.is_empty());
        assert_eq!(verified_empty.root.reply_count, 0);

        // Conversation last_msg updated from newest reply
        let conv = storage.get_conversation(conv_key).unwrap().unwrap();
        assert_eq!(conv.last_msg_id.as_deref(), Some("m1-r2"));
        assert_eq!(conv.last_msg_author.as_deref(), Some("Bob"));
        assert_eq!(conv.last_msg_preview.as_deref(), Some("Reply 2"));
    }

    #[test]
    fn test_fts5_trigram_and_two_char_korean_like_fallback() {
        let storage = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c1", "프로젝트방", false, None);
        let conv_key = chat.conv_key.clone();
        storage.upsert_conversations(&[chat]).unwrap();

        let m1 = sample_msg(
            &conv_key,
            "m1",
            None,
            "홍길동",
            "오늘 오후 3시에 배포 일정 회의가 있습니다. Kubernetes rollout 점검 부탁드립니다.",
            Some("주간 배포"),
            "2026-11-15T09:00:00Z",
        );
        let mut m2 = sample_msg(
            &conv_key,
            "m2",
            None,
            "김영희",
            "삭제될 메시지입니다",
            None,
            "2026-11-15T10:00:00Z",
        );
        m2.deleted = true;
        storage.upsert_messages(&[m1, m2]).unwrap();

        // >= 3 chars Korean trigram
        let hits = storage.search_messages("배포 일정", None, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].msg_id, "m1");
        assert_eq!(hits[0].conv_title, "프로젝트방");

        // >= 3 chars English case-insensitive
        let eng_hits = storage.search_messages("kubernetes", Some(&conv_key), 10).unwrap();
        assert_eq!(eng_hits.len(), 1);
        assert!(eng_hits[0].snippet.contains("Kubernetes"));

        // < 3 chars Korean 2-character fallback
        let two_char_ko = storage.search_messages("회의", None, 10).unwrap();
        assert_eq!(two_char_ko.len(), 1);
        assert_eq!(two_char_ko[0].msg_id, "m1");

        let two_char_subj = storage.search_messages("주간", None, 10).unwrap();
        assert_eq!(two_char_subj.len(), 1);

        // Deleted message must not appear in search
        let deleted_hits = storage.search_messages("삭제될", None, 10).unwrap();
        assert!(deleted_hits.is_empty());

        // Re-upsert m1 with updated body keeps FTS synchronized without duplicate rows
        let mut m1_updated = sample_msg(
            &conv_key,
            "m1",
            None,
            "홍길동",
            "새로운 내용으로 수정됨",
            None,
            "2026-11-15T09:00:00Z",
        );
        m1_updated.last_modified_at = Some("2026-11-15T09:10:00Z".to_string());
        storage.upsert_messages(&[m1_updated]).unwrap();
        assert!(storage.search_messages("Kubernetes", None, 10).unwrap().is_empty());
        assert_eq!(storage.search_messages("수정됨", None, 10).unwrap().len(), 1);
    }

    #[test]
    fn test_members_presence_mentions_and_cursors() {
        let storage = Storage::open_in_memory().unwrap();
        let conv_key = "chat:c1";
        storage
            .upsert_members(
                conv_key,
                &[MemberInfo {
                    user_id: "u1".to_string(),
                    member_id: Some("mem1".to_string()),
                    display_name: "Alice".to_string(),
                    email: Some("alice@example.com".to_string()),
                    roles: vec!["owner".to_string()],
                    presence: None,
                }],
            )
            .unwrap();
        storage
            .upsert_presences(&[PresenceStatus {
                user_id: "u1".to_string(),
                availability: "Available".to_string(),
                activity: "Available".to_string(),
            }])
            .unwrap();

        let members = storage.list_members(conv_key).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(
            members[0].presence.as_ref().map(|p| p.availability.as_str()),
            Some("Available")
        );

        let mut msg = sample_msg(
            conv_key,
            "m10",
            None,
            "Bob",
            "@Alice please check",
            None,
            "2026-11-15T12:00:00Z",
        );
        msg.mentions.push(MentionInfo {
            id: 0,
            user_id: "u1".to_string(),
            display_name: "Alice".to_string(),
        });
        storage.upsert_messages(&[msg]).unwrap();
        let mentions = storage.list_mentions_for_user("u1", 10).unwrap();
        assert_eq!(mentions.len(), 1);
        assert_eq!(mentions[0].msg_id, "m10");

        let cursor = SyncCursor {
            scope_key: "chan:t1:ch1".to_string(),
            delta_link: Some("https://graph.microsoft.com/v1.0/delta?token=abc".to_string()),
            oldest_msg_ts: Some("2026-11-01T00:00:00Z".to_string()),
            synced_at: "2026-11-15T12:00:00Z".to_string(),
        };
        storage.save_sync_cursor(&cursor).unwrap();
        let loaded = storage.get_sync_cursor("chan:t1:ch1").unwrap().unwrap();
        assert_eq!(loaded, cursor);
    }

    #[test]
    fn test_file_backed_wal_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("teams.db");
        {
            let storage = Storage::open(&db_file).unwrap();
            let chat = sample_chat("c-disk", "Disk Room", false, Some("2026-11-15T12:00:00Z"));
            storage.upsert_conversations(&[chat]).unwrap();
        }
        let reopened = Storage::open(&db_file).unwrap();
        let loaded = reopened.get_conversation("chat:c-disk").unwrap().unwrap();
        assert_eq!(loaded.title, "Disk Room");
    }

    #[test]
    fn test_file_backed_reader_pool_concurrent_with_writer() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("teams-concurrent.db");
        let storage = Storage::open(&db_file).unwrap();

        let chat = sample_chat("c-wal", "Concurrency Test Room", true, Some("2026-11-15T12:00:00Z"));
        let conv_key = chat.conv_key.clone();
        storage.upsert_conversations(&[chat]).unwrap();
        let m1 = sample_msg(
            &conv_key,
            "m1",
            None,
            "Alice",
            "WAL reader pool concurrent read verification message",
            Some("Performance test"),
            "2026-11-15T12:00:00Z",
        );
        storage.upsert_messages(&[m1]).unwrap();

        // Hold the writer mutex while 6 reader threads query concurrently via the WAL reader pool.
        {
            let _writer_guard = storage.conn().lock().unwrap();
            std::thread::scope(|s| {
                for _ in 0..6 {
                    s.spawn(|| {
                        let convs = storage.list_conversations().unwrap();
                        assert_eq!(convs.len(), 1);
                        assert_eq!(convs[0].title, "Concurrency Test Room");

                        let roots = storage.list_root_messages(&conv_key, 10, None).unwrap();
                        assert_eq!(roots.len(), 1);
                        assert_eq!(roots[0].msg_id, "m1");

                        let msg = storage.get_message(&conv_key, "m1").unwrap().unwrap();
                        assert_eq!(
                            msg.body_plain,
                            "WAL reader pool concurrent read verification message"
                        );

                        let hits = storage.search_messages("concurrent read", None, 10).unwrap();
                        assert_eq!(hits.len(), 1);
                        assert_eq!(hits[0].msg_id, "m1");
                    });
                }
            });
        }

        let pooled_count = storage.readers.as_ref().unwrap().lock().unwrap().len();
        assert!((1..=MAX_READER_POOL_SIZE).contains(&pooled_count));

        // Subsequent write is visible to pooled readers.
        let m2 = sample_msg(
            &conv_key,
            "m2",
            None,
            "Bob",
            "Second committed message",
            None,
            "2026-11-15T12:05:00Z",
        );
        storage.upsert_messages(&[m2]).unwrap();
        let roots_after = storage.list_root_messages(&conv_key, 10, None).unwrap();
        assert_eq!(roots_after.len(), 2);
        assert_eq!(roots_after[1].msg_id, "m2");
    }

    #[test]
    fn test_upsert_messages_idempotency_and_partial_fts_skip() {
        let storage = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c-idem", "Idempotency Test", false, None);
        let conv_key = chat.conv_key.clone();
        storage.upsert_conversations(&[chat]).unwrap();

        let mut m1 = sample_msg(
            &conv_key,
            "m1",
            None,
            "Alice",
            "Initial body Kubernetes deployment notice",
            Some("Deploy notice"),
            "2026-11-15T09:00:00Z",
        );
        assert_eq!(storage.upsert_messages(&[m1.clone()]).unwrap(), 1);
        assert_eq!(
            storage.search_messages("Kubernetes", None, 10).unwrap().len(),
            1
        );

        // 1) Identical re-upsert must skip both messages and messages_fts writes (0 SQLite changes).
        let changes_before_noop = storage.conn().lock().unwrap().total_changes();
        assert_eq!(storage.upsert_messages(&[m1.clone()]).unwrap(), 0);
        let changes_after_noop = storage.conn().lock().unwrap().total_changes();
        assert_eq!(changes_after_noop - changes_before_noop, 0);
        assert_eq!(
            storage.search_messages("Kubernetes", None, 10).unwrap().len(),
            1
        );

        // 2) Metadata-only change (reply_count / last_modified_at) updates messages without FTS churn.
        m1.reply_count = 3;
        m1.last_modified_at = Some("2026-11-15T09:02:00Z".to_string());
        let changes_before_meta = storage.conn().lock().unwrap().total_changes();
        assert_eq!(storage.upsert_messages(&[m1.clone()]).unwrap(), 1);
        let changes_after_meta = storage.conn().lock().unwrap().total_changes();
        assert_eq!(changes_after_meta - changes_before_meta, 1);
        assert_eq!(
            storage.get_message(&conv_key, "m1").unwrap().unwrap().reply_count,
            3
        );
        assert_eq!(
            storage.search_messages("Kubernetes", None, 10).unwrap().len(),
            1
        );

        // 3) Body change updates both messages and messages_fts.
        m1.body_plain = "Edited body PostgreSQL check".to_string();
        m1.last_modified_at = Some("2026-11-15T09:05:00Z".to_string());
        storage.upsert_messages(&[m1.clone()]).unwrap();
        assert!(storage.search_messages("Kubernetes", None, 10).unwrap().is_empty());
        assert_eq!(
            storage.search_messages("PostgreSQL", None, 10).unwrap().len(),
            1
        );

        // 4) Soft delete removes from messages_fts while updating messages.
        m1.deleted = true;
        m1.last_modified_at = Some("2026-11-15T09:10:00Z".to_string());
        storage.upsert_messages(&[m1]).unwrap();
        assert!(storage.get_message(&conv_key, "m1").unwrap().unwrap().deleted);
        assert!(storage.search_messages("PostgreSQL", None, 10).unwrap().is_empty());
    }

    #[test]
    fn test_shrink_memory_in_memory_and_file_backed_with_reader_pool() {
        // 1) In-memory Storage (writer connection only)
        let mem = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c-mem-shrink", "Memory Shrink", false, None);
        mem.upsert_conversations(&[chat]).unwrap();
        mem.shrink_memory();
        assert_eq!(mem.list_conversations().unwrap().len(), 1);

        // 2) File-backed WAL Storage with active pooled reader connections
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("shrink.db");
        let wal = Storage::open(&db_file).unwrap();
        let wal_chat = sample_chat("c-wal-shrink", "WAL Shrink", false, None);
        let conv_key = wal_chat.conv_key.clone();
        wal.upsert_conversations(&[wal_chat]).unwrap();
        let msg = sample_msg(
            &conv_key,
            "m1",
            None,
            "Alice",
            "SQLite shrink_memory test body",
            None,
            "2026-11-15T12:00:00Z",
        );
        wal.upsert_messages(&[msg]).unwrap();

        // Populate pooled reader connections before calling shrink_memory
        assert_eq!(wal.list_conversations().unwrap().len(), 1);
        assert_eq!(wal.list_root_messages(&conv_key, 10, None).unwrap().len(), 1);
        let pooled_count = wal.readers.as_ref().unwrap().lock().unwrap().len();
        assert!(pooled_count >= 1);

        wal.shrink_memory();

        // Both writer and pooled readers continue operating cleanly afterwards
        let msg2 = sample_msg(
            &conv_key,
            "m2",
            None,
            "Bob",
            "Verify subsequent read/write after shrink_memory",
            None,
            "2026-11-15T12:05:00Z",
        );
        wal.upsert_messages(&[msg2]).unwrap();
        assert_eq!(wal.list_root_messages(&conv_key, 10, None).unwrap().len(), 2);
    }

    #[test]
    fn test_list_older_root_messages_tie_breaking_same_timestamp() {
        let storage = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c-tie", "Same Timestamp Room", false, None);
        let conv_key = chat.conv_key.clone();
        storage.upsert_conversations(&[chat]).unwrap();

        let ts = "2026-11-15T10:00:00Z";
        let m0 = sample_msg(&conv_key, "m0", None, "Bob", "earlier", None, "2026-11-15T09:59:59Z");
        let m1 = sample_msg(&conv_key, "m1", None, "Bob", "first", None, ts);
        let m2 = sample_msg(&conv_key, "m2", None, "Carol", "second", None, ts);
        let m3 = sample_msg(&conv_key, "m3", None, "Dave", "third", None, ts);
        // Thread reply with the same timestamp must not appear in root queries
        let reply = sample_msg(&conv_key, "m2-r1", Some("m2"), "Alice", "reply", None, ts);
        storage.upsert_messages(&[m3, m1, m0, m2, reply]).unwrap();

        // Newest 2 root messages in chronological order -> ["m2", "m3"]
        let latest = storage.list_root_messages(&conv_key, 2, None).unwrap();
        let latest_ids: Vec<&str> = latest.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(latest_ids, vec!["m2", "m3"]);

        // Paginate backwards 1 by 1 with identical created_at tie-breaker
        let page1 = storage
            .list_older_root_messages(&conv_key, 1, ts, Some("m3"))
            .unwrap();
        assert_eq!(page1.len(), 1);
        assert_eq!(page1[0].msg_id, "m2");

        let page2 = storage
            .list_older_root_messages(&conv_key, 1, ts, Some("m2"))
            .unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].msg_id, "m1");

        let page3 = storage
            .list_older_root_messages(&conv_key, 1, ts, Some("m1"))
            .unwrap();
        assert_eq!(page3.len(), 1);
        assert_eq!(page3[0].msg_id, "m0");

        assert!(storage
            .list_older_root_messages(&conv_key, 1, "2026-11-15T09:59:59Z", Some("m0"))
            .unwrap()
            .is_empty());

        // Multi-item page with before_msg_id = Some("m3") preserves chronological order
        let batch = storage
            .list_older_root_messages(&conv_key, 2, ts, Some("m3"))
            .unwrap();
        let batch_ids: Vec<&str> = batch.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(batch_ids, vec!["m1", "m2"]);

        // Without before_msg_id, strict created_at < ts skips same-timestamp roots and returns m0
        let without_id = storage
            .list_older_root_messages(&conv_key, 10, ts, None)
            .unwrap();
        let without_ids: Vec<&str> = without_id.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(without_ids, vec!["m0"]);
    }

    #[test]
    fn test_open_readonly_does_not_create_file_and_reads_existing() {
        let dir = tempfile::tempdir().unwrap();
        let missing_db = dir.path().join("nonexistent/cache.db");
        assert!(Storage::open_readonly(&missing_db).is_err());
        assert!(!missing_db.exists());

        let db_file = dir.path().join("existing.db");
        {
            let rw = Storage::open(&db_file).unwrap();
            let chat = sample_chat("c-ro", "Readonly Room", false, Some("2026-11-15T12:00:00Z"));
            rw.upsert_conversations(&[chat]).unwrap();
        }
        let ro = Storage::open_readonly(&db_file).unwrap();
        let convs = ro.list_conversations().unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, "Readonly Room");
    }

    #[test]
    fn test_upsert_attachment_mention_edit_and_soft_delete_reply_count() {
        use crate::models::AttachmentInfo;

        let storage = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c-att", "Mentions & Replies", false, None);
        let conv_key = chat.conv_key.clone();
        storage.upsert_conversations(&[chat]).unwrap();

        let mut root = sample_msg(&conv_key, "root1", None, "Alice", "Root", None, "2026-11-15T09:00:00Z");
        let rep1 = sample_msg(&conv_key, "r1", Some("root1"), "Bob", "Reply 1", None, "2026-11-15T09:01:00Z");
        let mut rep2 = sample_msg(&conv_key, "r2", Some("root1"), "Carol", "Reply 2", None, "2026-11-15T09:02:00Z");
        assert_eq!(storage.upsert_messages(&[root.clone(), rep1, rep2.clone()]).unwrap(), 3);
        assert_eq!(storage.get_message(&conv_key, "root1").unwrap().unwrap().reply_count, 2);

        // Soft-deleting rep2 updates root reply_count accurately from 2 -> 1
        rep2.deleted = true;
        assert_eq!(storage.upsert_messages(&[rep2]).unwrap(), 1);
        assert_eq!(storage.get_message(&conv_key, "root1").unwrap().unwrap().reply_count, 1);

        // Editing attachments & mentions without changing last_modified_at is NOT skipped
        root.attachments.push(AttachmentInfo {
            id: "att1".to_string(),
            content_type: "reference".to_string(),
            content_url: None,
            name: Some("doc.pdf".to_string()),
            content_json: None,
            thumbnail_url: None,
        });
        assert_eq!(storage.upsert_messages(&[root.clone()]).unwrap(), 1);
        assert_eq!(storage.get_message(&conv_key, "root1").unwrap().unwrap().attachments.len(), 1);

        root.mentions.push(MentionInfo {
            id: 0,
            user_id: "u-target".to_string(),
            display_name: "Target".to_string(),
        });
        assert_eq!(storage.upsert_messages(&[root]).unwrap(), 1);
        let mentioned = storage.list_mentions_for_user("u-target", 10).unwrap();
        assert_eq!(mentioned.len(), 1);
        assert_eq!(mentioned[0].msg_id, "root1");
    }

    #[test]
    fn test_upsert_messages_returning_changed_ids_and_get_messages_by_ids() {
        let storage = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c-inc", "Incremental Chat", false, None);
        let conv_key = chat.conv_key.clone();
        storage.upsert_conversations(&[chat]).unwrap();

        let m1 = sample_msg(&conv_key, "m1", None, "Alice", "one", None, "2026-11-20T01:00:00Z");
        let m2 = sample_msg(&conv_key, "m2", None, "Bob", "two", None, "2026-11-20T02:00:00Z");

        // Initial insert returns both IDs; duplicates in the same batch are deduplicated.
        let changed = storage
            .upsert_messages_returning_changed_ids(&[m1.clone(), m2.clone(), m1.clone()])
            .unwrap();
        assert_eq!(changed, vec!["m1".to_string(), "m2".to_string()]);

        // Re-upserting identical rows returns empty Vec.
        let unchanged = storage
            .upsert_messages_returning_changed_ids(&[m1.clone(), m2.clone()])
            .unwrap();
        assert!(unchanged.is_empty());

        let mut html_only = m1.clone();
        html_only.body_html = "<p><strong>one</strong></p>".to_string();
        assert_eq!(
            storage
                .upsert_messages_returning_changed_ids(std::slice::from_ref(&html_only))
                .unwrap(),
            vec!["m1"]
        );
        assert_eq!(
            storage.get_message(&conv_key, "m1").unwrap().unwrap().body_html,
            html_only.body_html
        );

        // Editing one row and soft-deleting another returns only changed IDs.
        let mut m1_edit = m1.clone();
        m1_edit.body_plain = "one edited".to_string();
        m1_edit.body_html = "<p>one edited</p>".to_string();
        m1_edit.last_modified_at = Some("2026-11-20T02:05:00Z".to_string());
        let mut m2_del = m2.clone();
        m2_del.deleted = true;
        let m3 = sample_msg(&conv_key, "m3", None, "Alice", "three", None, "2026-11-20T03:00:00Z");

        let changed2 = storage
            .upsert_messages_returning_changed_ids(&[m1_edit, m2_del, m3])
            .unwrap();
        assert_eq!(
            changed2,
            vec!["m1".to_string(), "m2".to_string(), "m3".to_string()]
        );

        // get_messages_by_ids reads only requested unique IDs via get_message.
        repo::reset_mapped_message_rows();
        let empty = storage.get_messages_by_ids(&conv_key, &[]).unwrap();
        assert!(empty.is_empty());
        assert_eq!(repo::mapped_message_rows(), 0);

        repo::reset_mapped_message_rows();
        let fetched = storage
            .get_messages_by_ids(
                &conv_key,
                &["m1".to_string(), "m2".to_string(), "m1".to_string()],
            )
            .unwrap();
        assert_eq!(repo::mapped_message_rows(), 2);
        assert_eq!(fetched.len(), 2);
        assert_eq!(fetched[0].msg_id, "m1");
        assert_eq!(fetched[0].body_plain, "one edited");
        assert_eq!(fetched[1].msg_id, "m2");
        assert!(fetched[1].deleted);
    }

    #[test]
    fn test_local_edit_soft_delete_reaction_and_stable_last_msg_preview() {
        let storage = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c-local", "Local Write Room", false, None);
        let conv_key = chat.conv_key.clone();
        storage.upsert_conversations(&[chat]).unwrap();

        let mut m1 = sample_msg(&conv_key, "m1", None, "Alice", "older-token", None, "2026-11-20T01:00:00Z");
        m1.body_html = "<p><at id=\"0\">Alice</at> older-token</p>".to_string();
        m1.mentions = vec![crate::models::MentionInfo {
            id: 0,
            user_id: "u-alice".to_string(),
            display_name: "Alice".to_string(),
        }];
        let m2 = sample_msg(&conv_key, "m2", None, "Alice", "alpha-before-edit", None, "2026-11-20T02:00:00Z");
        storage.upsert_messages(&[m1, m2]).unwrap();
        assert_eq!(
            storage.get_conversation(&conv_key).unwrap().unwrap().last_msg_preview.as_deref(),
            Some("alpha-before-edit")
        );

        // 1) Editing an older message keeps last_msg_preview pointing at m2 and drops stale mentions.
        let (edited_m1, _) = storage
            .apply_local_edit(&conv_key, "m1", "<p>older-edited</p>", "2026-11-20T02:05:00Z")
            .unwrap()
            .unwrap();
        assert!(edited_m1.mentions.is_empty(), "stale mentions must be cleared on full edit");
        assert!(
            storage
                .get_message(&conv_key, "m1")
                .unwrap()
                .unwrap()
                .mentions
                .is_empty()
        );
        assert_eq!(
            storage.get_conversation(&conv_key).unwrap().unwrap().last_msg_preview.as_deref(),
            Some("alpha-before-edit")
        );
        assert!(storage.search_messages("older-token", None, 10).unwrap().is_empty());
        assert_eq!(storage.search_messages("older-edited", None, 10).unwrap().len(), 1);

        // 2) Editing the newest message updates FTS5 and conversations.last_msg_preview.
        let (_, edit_out) = storage
            .apply_local_edit(&conv_key, "m2", "<p>beta-after-edit</p>", "2026-11-20T02:10:00Z")
            .unwrap()
            .unwrap();
        assert_eq!(edit_out.changed_ids, vec!["m2".to_string()]);
        assert_eq!(edit_out.preview_updated_convs, vec![conv_key.clone()]);
        assert!(storage.search_messages("alpha-before-edit", None, 10).unwrap().is_empty());
        assert_eq!(storage.search_messages("beta-after-edit", None, 10).unwrap().len(), 1);
        assert_eq!(
            storage.get_conversation(&conv_key).unwrap().unwrap().last_msg_preview.as_deref(),
            Some("beta-after-edit")
        );

        // 3) Reaction toggle is persisted and idempotent.
        storage
            .apply_local_reaction(&conv_key, "m2", "heart", true, Some("Me"))
            .unwrap()
            .unwrap();
        storage
            .apply_local_reaction(&conv_key, "m2", "heart", true, Some("Me"))
            .unwrap()
            .unwrap();
        let after_rx = storage.get_message(&conv_key, "m2").unwrap().unwrap();
        assert_eq!(after_rx.reactions.len(), 1);
        assert_eq!(after_rx.reactions[0].count, 1);
        assert!(after_rx.reactions[0].me_reacted);
        storage
            .apply_local_reaction(&conv_key, "m2", "heart", false, Some("Me"))
            .unwrap()
            .unwrap();
        assert!(storage.get_message(&conv_key, "m2").unwrap().unwrap().reactions.is_empty());

        // 4) Soft-deleting the newest message clears FTS5 and sets preview to "(deleted message)".
        storage
            .apply_local_soft_delete(&conv_key, "m2", "2026-11-20T02:15:00Z")
            .unwrap()
            .unwrap();
        assert!(storage.search_messages("beta-after-edit", None, 10).unwrap().is_empty());
        let deleted_m2 = storage.get_message(&conv_key, "m2").unwrap().unwrap();
        assert!(deleted_m2.deleted);
        assert_eq!(deleted_m2.body_plain, "(deleted message)");
        assert_eq!(
            storage.get_conversation(&conv_key).unwrap().unwrap().last_msg_preview.as_deref(),
            Some("(deleted message)")
        );
    }

    #[test]
    fn test_list_thread_replies_returns_newest_limit_in_chronological_order() {
        let storage = Storage::open_in_memory().unwrap();
        let conv_key = "chan:t1:ch1";
        let root = sample_msg(conv_key, "root", None, "Alice", "root post", None, "2026-11-20T05:00:00Z");
        let mut all = vec![root];
        for i in 1..=12 {
            all.push(sample_msg(
                conv_key,
                &format!("rep-{i:02}"),
                Some("root"),
                "Bob",
                &format!("reply {i:02}"),
                None,
                &format!("2026-11-20T05:{i:02}:00Z"),
            ));
        }
        storage.upsert_messages(&all).unwrap();

        let newest_5 = storage.list_thread_replies(conv_key, "root", 5).unwrap();
        let ids: Vec<&str> = newest_5.iter().map(|m| m.msg_id.as_str()).collect();
        assert_eq!(ids, vec!["rep-08", "rep-09", "rep-10", "rep-11", "rep-12"]);
    }

    #[test]
    fn fetched_thread_reports_coverage() {
        let storage = Storage::open_in_memory().unwrap();
        let conv_key = "chan:t1:ch1";
        let root = sample_msg(conv_key, "m1", None, "Alice", "root", None, "2026-11-20T09:00:00Z");
        let r1 = sample_msg(conv_key, "r1", Some("m1"), "Bob", "reply 1", None, "2026-11-20T09:01:00Z");
        let r2 = sample_msg(conv_key, "r2", Some("m1"), "Bob", "reply 2", None, "2026-11-20T09:02:00Z");
        let r3 = sample_msg(conv_key, "r3", Some("m1"), "Bob", "reply 3", None, "2026-11-20T09:03:00Z");

        // 1) Incomplete window of newest 2 replies
        let partial = ThreadView {
            conv_key: conv_key.to_string(),
            root: root.clone(),
            replies: vec![r2.clone(), r3.clone()],
        };
        storage.save_fetched_thread(&partial, false).unwrap();
        let (t1, cov1) = storage.get_fetched_thread(conv_key, "m1", 1).unwrap().unwrap();
        assert_eq!(t1.replies.len(), 1);
        assert_eq!(t1.replies[0].msg_id, "r3");
        assert_eq!(cov1, ThreadCoverage { has_more: true, reply_total: None });
        let (t2, cov2) = storage.get_fetched_thread(conv_key, "m1", 2).unwrap().unwrap();
        assert_eq!(t2.replies.len(), 2);
        assert_eq!(cov2, ThreadCoverage { has_more: true, reply_total: None });
        assert!(storage.get_fetched_thread(conv_key, "m1", 3).unwrap().is_none());

        // 2) Complete window of all 3 replies
        let full = ThreadView {
            conv_key: conv_key.to_string(),
            root: root.clone(),
            replies: vec![r1, r2, r3],
        };
        storage.save_fetched_thread(&full, true).unwrap();
        let (t2c, cov2c) = storage.get_fetched_thread(conv_key, "m1", 2).unwrap().unwrap();
        assert_eq!(t2c.replies.len(), 2);
        assert_eq!(cov2c, ThreadCoverage { has_more: true, reply_total: Some(3) });
        let (t3c, cov3c) = storage.get_fetched_thread(conv_key, "m1", 3).unwrap().unwrap();
        assert_eq!(t3c.replies.len(), 3);
        assert_eq!(cov3c, ThreadCoverage { has_more: false, reply_total: Some(3) });
        let (t50c, cov50c) = storage.get_fetched_thread(conv_key, "m1", 50).unwrap().unwrap();
        assert_eq!(t50c.replies.len(), 3);
        assert_eq!(cov50c, ThreadCoverage { has_more: false, reply_total: Some(3) });

        // 3) Verified empty thread (0 replies, complete)
        let empty = ThreadView {
            conv_key: conv_key.to_string(),
            root,
            replies: Vec::new(),
        };
        storage.save_fetched_thread(&empty, true).unwrap();
        let (te, cove) = storage.get_fetched_thread(conv_key, "m1", 50).unwrap().unwrap();
        assert!(te.replies.is_empty());
        assert_eq!(cove, ThreadCoverage { has_more: false, reply_total: Some(0) });
    }

    // TEAMS-27: a shallow cursor must not replace a deeper one.
    #[test]
    fn regression_teams_27_history_cursor_is_only_replaced_by_a_deeper_one() {
        let storage = Storage::open_in_memory().unwrap();
        let scope = SyncCursor::history_scope("chat:c1");
        let cursor = |link: Option<&str>, oldest: &str| SyncCursor {
            scope_key: scope.clone(),
            delta_link: link.map(str::to_string),
            oldest_msg_ts: Some(oldest.to_string()),
            synced_at: "2026-11-20T00:00:00Z".to_string(),
        };
        let stored = |st: &Storage| st.get_sync_cursor(&scope).unwrap().unwrap();

        // No cursor yet: saved.
        assert!(storage.save_history_cursor_if_deeper(&cursor(Some("page2"), "2026-11-20T10:00:00Z")).unwrap());
        // Deeper: saved. Timestamps are compared as instants, whatever their spelling.
        assert!(storage
            .save_history_cursor_if_deeper(&cursor(Some("page5"), "2026-11-18T10:00:00.000000000Z"))
            .unwrap());
        // Shallower or equally deep: the stored cursor stays.
        assert!(!storage.save_history_cursor_if_deeper(&cursor(Some("page2"), "2026-11-20T10:00:00Z")).unwrap());
        assert!(!storage.save_history_cursor_if_deeper(&cursor(Some("page5b"), "2026-11-18T10:00:00Z")).unwrap());
        assert_eq!(stored(&storage).delta_link.as_deref(), Some("page5"));

        // EOF always wins, and is never replaced by a page cursor.
        assert!(storage.save_history_cursor_if_deeper(&cursor(None, SyncCursor::HISTORY_EOF)).unwrap());
        assert!(stored(&storage).is_history_eof());
        assert!(!storage.save_history_cursor_if_deeper(&cursor(Some("page2"), "2026-11-20T10:00:00Z")).unwrap());
        assert!(!storage.save_history_cursor_if_deeper(&cursor(Some("page9"), "2026-11-01T10:00:00Z")).unwrap());
        assert!(stored(&storage).is_history_eof());
    }

    // TEAMS-38: sync writes Graph-derived summaries, which always carry `pinned: false`.
    #[test]
    fn regression_teams_38_upsert_keeps_local_pin() {
        let storage = Storage::open_in_memory().unwrap();
        let chat = sample_chat("c1", "Chat", false, Some("2026-11-20T09:00:00Z"));
        storage.upsert_conversations(std::slice::from_ref(&chat)).unwrap();
        let pinned = |st: &Storage| st.get_conversation(&chat.conv_key).unwrap().unwrap().pinned;
        assert!(!pinned(&storage));

        storage.set_conversation_pinned(&chat.conv_key, true).unwrap();
        storage.upsert_conversations(std::slice::from_ref(&chat)).unwrap();
        assert!(pinned(&storage), "an upsert must not clear a local pin");

        storage.set_conversation_pinned(&chat.conv_key, false).unwrap();
        assert!(!pinned(&storage));
        storage.upsert_conversations(std::slice::from_ref(&chat)).unwrap();
        assert!(!pinned(&storage));

        // An upsert can still set a pin.
        let mut pinned_chat = chat.clone();
        pinned_chat.pinned = true;
        storage.upsert_conversations(&[pinned_chat]).unwrap();
        assert!(pinned(&storage));
    }

    // TEAMS-23: a thread read online once and then extended by sync.
    #[test]
    fn regression_teams_23_complete_thread_window_includes_synced_replies() {
        let storage = Storage::open_in_memory().unwrap();
        let conv_key = "chan:t1:ch1";
        let reply_ids = |view: &ThreadView| -> Vec<String> {
            view.replies.iter().map(|m| m.msg_id.clone()).collect()
        };
        let root = sample_msg(conv_key, "m1", None, "Alice", "root", None, "2026-11-20T09:00:00Z");
        let r1 = sample_msg(conv_key, "r1", Some("m1"), "Bob", "reply 1", None, "2026-11-20T09:01:00Z");
        let r2 = sample_msg(conv_key, "r2", Some("m1"), "Bob", "reply 2", None, "2026-11-20T09:02:00Z");
        let fetched = ThreadView {
            conv_key: conv_key.to_string(),
            root,
            replies: vec![r1, r2],
        };
        storage.save_fetched_thread(&fetched, true).unwrap();

        // Sync delivers a newer reply; no online thread read happens in between.
        let r3 = sample_msg(conv_key, "r3", Some("m1"), "Bob", "reply 3", None, "2026-11-20T09:03:00Z");
        storage.upsert_messages(&[r3]).unwrap();

        let (all, cov) = storage.get_fetched_thread(conv_key, "m1", 50).unwrap().unwrap();
        assert_eq!(reply_ids(&all), ["r1", "r2", "r3"]);
        assert_eq!(all.root.reply_count, 3);
        assert_eq!(cov, ThreadCoverage { has_more: false, reply_total: Some(3) });

        let (newest, cov) = storage.get_fetched_thread(conv_key, "m1", 2).unwrap().unwrap();
        assert_eq!(reply_ids(&newest), ["r2", "r3"]);
        assert_eq!(cov, ThreadCoverage { has_more: true, reply_total: Some(3) });

        // A tombstone recorded later lowers the count; the row itself is still returned.
        storage
            .apply_local_soft_delete(conv_key, "r2", "2026-11-20T09:10:00Z")
            .unwrap()
            .unwrap();
        let (after_delete, _) = storage.get_fetched_thread(conv_key, "m1", 50).unwrap().unwrap();
        assert_eq!(after_delete.root.reply_count, 2);
        assert_eq!(reply_ids(&after_delete), ["r1", "r2", "r3"]);
        assert!(after_delete.replies[1].deleted);

        // A complete window stays authoritative up to its newest reply: a cached row that
        // sorts before it and was not part of the snapshot is not resurrected.
        let stray = sample_msg(conv_key, "r0", Some("m1"), "Bob", "stale", None, "2026-11-20T09:00:30Z");
        storage.upsert_messages(&[stray]).unwrap();
        let (no_stray, _) = storage.get_fetched_thread(conv_key, "m1", 50).unwrap().unwrap();
        assert_eq!(reply_ids(&no_stray), ["r1", "r2", "r3"]);

        // A thread that was never read online is still a cache miss.
        let other_root = sample_msg(conv_key, "m9", None, "Alice", "other", None, "2026-11-20T10:00:00Z");
        let other_reply = sample_msg(conv_key, "m9-r1", Some("m9"), "Bob", "x", None, "2026-11-20T10:01:00Z");
        storage.upsert_messages(&[other_root, other_reply]).unwrap();
        assert!(storage.get_fetched_thread(conv_key, "m9", 50).unwrap().is_none());
    }

    // TEAMS-20: left teams and deleted channels leave the cache, but only on evidence.
    #[test]
    fn regression_teams_20_joined_teams_prune_absent_teams_and_channels() {
        use crate::models::TeamChannelError;

        let storage = Storage::open_in_memory().unwrap();
        let channel = |team: &str, id: &str| ChannelSummary {
            id: id.to_string(),
            team_id: team.to_string(),
            team_name: format!("Team {team}"),
            display_name: id.to_string(),
            description: None,
            membership_type: "standard".to_string(),
            web_url: None,
        };
        let team = |id: &str, channels: Vec<ChannelSummary>| TeamSummary {
            id: id.to_string(),
            display_name: format!("Team {id}"),
            description: None,
            channels,
        };
        storage
            .upsert_teams(&[
                team("ta", vec![channel("ta", "keep"), channel("ta", "deleted")]),
                team("tb", vec![channel("tb", "secret")]),
                team("tgone", vec![channel("tgone", "old")]),
            ])
            .unwrap();
        let chat = sample_chat("c1", "Chat", false, Some("2026-11-20T09:00:00Z"));
        storage.upsert_conversations(std::slice::from_ref(&chat)).unwrap();
        storage
            .upsert_messages(&[
                sample_msg("chan:ta:deleted", "m1", None, "Alice", "orphan one", None, "2026-11-20T09:00:00Z"),
                sample_msg("chan:tgone:old", "m2", None, "Alice", "orphan two", None, "2026-11-20T09:00:00Z"),
            ])
            .unwrap();
        let conv_keys = |st: &Storage| -> Vec<String> {
            let mut keys: Vec<String> =
                st.list_conversations().unwrap().into_iter().map(|c| c.conv_key).collect();
            keys.sort();
            keys
        };
        let team_ids = |st: &Storage| -> Vec<String> {
            let mut ids: Vec<String> = st.list_teams().unwrap().into_iter().map(|t| t.id).collect();
            ids.sort();
            ids
        };
        let before = conv_keys(&storage);

        // An empty fetch is indistinguishable from a failed one: nothing is removed.
        storage
            .upsert_joined_teams(&JoinedTeams {
                teams: Vec::new(),
                channel_errors: Vec::new(),
            })
            .unwrap();
        assert_eq!(conv_keys(&storage), before);
        assert_eq!(team_ids(&storage), ["ta", "tb", "tgone"]);

        // Graph now lists ta (one channel left) and tb (channel fetch failed); tgone is gone.
        let fetched = JoinedTeams {
            teams: vec![team("ta", vec![channel("ta", "keep")]), team("tb", vec![])],
            channel_errors: vec![TeamChannelError {
                team_id: "tb".to_string(),
                display_name: "Team tb".to_string(),
                status: Some(429),
                error: "429 Too Many Requests".to_string(),
            }],
        };
        let merged = storage.upsert_joined_teams(&fetched).unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(team_ids(&storage), ["ta", "tb"]);
        assert_eq!(
            conv_keys(&storage),
            ["chan:ta:keep", "chan:tb:secret", "chat:c1"],
            "deleted channel and left team are gone; the failed team keeps its cached channel; chats are untouched"
        );
        let ta = storage.list_teams().unwrap().into_iter().find(|t| t.id == "ta").unwrap();
        assert_eq!(ta.channels.len(), 1);

        // Message rows of the removed conversations stay.
        assert!(storage.get_message("chan:ta:deleted", "m1").unwrap().is_some());
        assert!(storage.get_message("chan:tgone:old", "m2").unwrap().is_some());
    }

    #[test]
    fn upsert_joined_teams_keeps_cached_channels_for_failed_team() {
        use crate::models::TeamChannelError;

        let storage = Storage::open_in_memory().unwrap();
        let secret_ch = ChannelSummary {
            id: "secret".to_string(),
            team_id: "tb".to_string(),
            team_name: "Restricted".to_string(),
            display_name: "Secret".to_string(),
            description: None,
            membership_type: "private".to_string(),
            web_url: None,
        };
        storage
            .upsert_teams(&[
                TeamSummary {
                    id: "tb".to_string(),
                    display_name: "Restricted".to_string(),
                    description: None,
                    channels: vec![secret_ch.clone()],
                },
                TeamSummary {
                    id: "tc".to_string(),
                    display_name: "EmptyTeam".to_string(),
                    description: None,
                    channels: vec![],
                },
            ])
            .unwrap();

        let fetched = JoinedTeams {
            teams: vec![
                TeamSummary {
                    id: "tb".to_string(),
                    display_name: "Restricted".to_string(),
                    description: None,
                    channels: vec![],
                },
                TeamSummary {
                    id: "tc".to_string(),
                    display_name: "EmptyTeam".to_string(),
                    description: None,
                    channels: vec![],
                },
                TeamSummary {
                    id: "tn".to_string(),
                    display_name: "NewFailed".to_string(),
                    description: None,
                    channels: vec![],
                },
            ],
            channel_errors: vec![
                TeamChannelError {
                    team_id: "tb".to_string(),
                    display_name: "Restricted".to_string(),
                    status: Some(429),
                    error: "429 Too Many Requests".to_string(),
                },
                TeamChannelError {
                    team_id: "tn".to_string(),
                    display_name: "NewFailed".to_string(),
                    status: Some(403),
                    error: "403 Forbidden".to_string(),
                },
            ],
        };

        let merged = storage.upsert_joined_teams(&fetched).unwrap();
        for list in [&merged, &storage.list_teams().unwrap()] {
            let tb = list.iter().find(|t| t.id == "tb").unwrap();
            assert_eq!(tb.channels, vec![secret_ch.clone()]);
            let tc = list.iter().find(|t| t.id == "tc").unwrap();
            assert!(tc.channels.is_empty());
            let tn = list.iter().find(|t| t.id == "tn").unwrap();
            assert!(tn.channels.is_empty());
        }
    }

    #[test]
    fn replace_members_drops_departed_and_orders_stably() {
        let storage = Storage::open_in_memory().unwrap();
        let mk = |uid: &str, name: &str, email: Option<&str>| MemberInfo {
            user_id: uid.to_string(),
            member_id: None,
            display_name: name.to_string(),
            email: email.map(str::to_string),
            roles: vec![],
            presence: None,
        };

        storage
            .upsert_members(
                "chat:c1",
                &[
                    mk("u2", "Bob", Some("b@x")),
                    mk("u1", "Bob", None),
                    mk("u3", "alice", None),
                ],
            )
            .unwrap();
        storage
            .upsert_members("chat:c2", &[mk("u9", "Other", None)])
            .unwrap();

        let before = storage.list_members("chat:c1").unwrap();
        let before_order: Vec<(&str, &str)> = before
            .iter()
            .map(|m| (m.user_id.as_str(), m.display_name.as_str()))
            .collect();
        assert_eq!(before_order, vec![("u1", "Bob"), ("u2", "Bob"), ("u3", "alice")]);

        storage
            .replace_members("chat:c1", &[mk("u2", "Bob", None), mk("u4", "Carol", None)])
            .unwrap();
        let after = storage.list_members("chat:c1").unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].user_id, "u2");
        assert_eq!(after[0].display_name, "Bob");
        assert_eq!(after[0].email.as_deref(), Some("b@x"));
        assert_eq!(after[1].user_id, "u4");
        assert_eq!(after[1].display_name, "Carol");

        storage.replace_members("chat:c1", &[]).unwrap();
        assert!(storage.list_members("chat:c1").unwrap().is_empty());
        assert_eq!(storage.list_members("chat:c2").unwrap().len(), 1);
    }

    // Track 5: channel delta sync returns roots without $expand=replies (reply count 0); the
    // count stored from an expanded page must survive both unchanged and edited roots.
    #[test]
    fn test_delta_root_upsert_preserves_hydrated_reply_count() {
        let storage = Storage::open_in_memory().unwrap();
        let conv_key = "chan:team-1:ch-1";
        let mut hydrated = sample_msg(
            conv_key,
            "root-1",
            None,
            "Alice",
            "Kickoff",
            Some("Plan"),
            "2026-11-20T09:00:00Z",
        );
        hydrated.reply_count = 4;
        storage.upsert_messages(&[hydrated.clone()]).unwrap();
        let stored_count = || {
            storage
                .get_message(conv_key, "root-1")
                .unwrap()
                .unwrap()
                .reply_count
        };
        assert_eq!(stored_count(), 4);

        // Delta repeats the root unchanged except for the missing replies.
        let mut delta_same = hydrated.clone();
        delta_same.reply_count = 0;
        storage.upsert_messages(&[delta_same]).unwrap();
        assert_eq!(stored_count(), 4);

        // Delta carries an edit: the row is rewritten, the count is kept.
        let mut delta_edited = hydrated.clone();
        delta_edited.reply_count = 0;
        delta_edited.body_plain = "Kickoff (edited)".to_string();
        delta_edited.body_html = "<p>Kickoff (edited)</p>".to_string();
        delta_edited.last_modified_at = Some("2026-11-20T10:00:00Z".to_string());
        storage.upsert_messages(&[delta_edited]).unwrap();
        let stored = storage.get_message(conv_key, "root-1").unwrap().unwrap();
        assert_eq!(stored.body_plain, "Kickoff (edited)");
        assert_eq!(stored.reply_count, 4);
        let roots = storage.list_root_messages(conv_key, 80, None).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].reply_count, 4, "the channel view reads the preserved count");

        // A later expanded page with more replies still raises the count.
        hydrated.reply_count = 6;
        hydrated.body_plain = "Kickoff (edited)".to_string();
        hydrated.body_html = "<p>Kickoff (edited)</p>".to_string();
        hydrated.last_modified_at = Some("2026-11-20T10:00:00Z".to_string());
        storage.upsert_messages(&[hydrated]).unwrap();
        assert_eq!(stored_count(), 6);
    }
}
