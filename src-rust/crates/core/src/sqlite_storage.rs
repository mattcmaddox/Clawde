// sqlite_storage.rs — Optional SQLite-backed session storage.
//
// Provides `SqliteSessionStore` as a faster, queryable alternative to
// the default JSONL storage.  Enabled by adding `rusqlite` to the
// crate's dependencies (already done via `features = ["bundled"]`).

use std::path::Path;

/// A persistent SQLite session + message store.
pub struct SqliteSessionStore {
    conn: rusqlite::Connection,
}

impl SqliteSessionStore {
    /// Open (or create) the database at `db_path` and ensure the schema exists.
    pub fn open(db_path: &Path) -> anyhow::Result<Self> {
        let conn = rusqlite::Connection::open(db_path)?;

        // The DB file holds session titles and full message content, which may
        // contain secrets read into context. Keep it owner-only (issue #212).
        crate::accounts::set_user_only_perms(db_path);

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS sessions (
                id          TEXT PRIMARY KEY,
                title       TEXT,
                model       TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL,
                message_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS messages (
                id          TEXT PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id),
                role        TEXT NOT NULL,
                content     TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                cost_usd    REAL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session
                ON messages(session_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_updated
                ON sessions(updated_at);
            ",
        )?;

        Ok(Self { conn })
    }

    /// Insert or replace a session record.  `created_at` is preserved on
    /// UPDATE so only `updated_at` changes.
    pub fn save_session(
        &self,
        session_id: &str,
        title: Option<&str>,
        model: &str,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO sessions (id, title, model, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 title      = excluded.title,
                 model      = excluded.model,
                 updated_at = excluded.updated_at",
            rusqlite::params![session_id, title, model, now],
        )?;
        Ok(())
    }

    /// Append a message to the given session (idempotent on `msg_id`).
    /// Also bumps `sessions.message_count` and `sessions.updated_at`.
    pub fn save_message(
        &self,
        session_id: &str,
        msg_id: &str,
        role: &str,
        content: &str,
        cost_usd: Option<f64>,
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        // Insert the message; ignore if already stored.
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO messages
             (id, session_id, role, content, created_at, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![msg_id, session_id, role, content, now, cost_usd],
        )?;
        // Only bump count when we actually inserted a new row.
        if inserted > 0 {
            self.conn.execute(
                "UPDATE sessions
                 SET updated_at    = ?1,
                     message_count = message_count + 1
                 WHERE id = ?2",
                rusqlite::params![now, session_id],
            )?;
        }
        Ok(())
    }

    /// Return the 100 most recently updated sessions.
    pub fn list_sessions(&self) -> anyhow::Result<Vec<SessionSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, model, created_at, updated_at, message_count
             FROM sessions
             ORDER BY updated_at DESC
             LIMIT 100",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                model: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                message_count: row.get::<_, Option<u32>>(5)?.unwrap_or(0),
            })
        })?;

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Full-text search across session titles and message content.
    /// Returns up to 50 matching sessions ordered by recency.
    pub fn search_sessions(&self, query: &str) -> anyhow::Result<Vec<SessionSummary>> {
        let like = format!("%{}%", query);
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT s.id, s.title, s.model,
                    s.created_at, s.updated_at, s.message_count
             FROM sessions s
             LEFT JOIN messages m ON m.session_id = s.id
             WHERE s.title LIKE ?1
                OR m.content LIKE ?1
             ORDER BY s.updated_at DESC
             LIMIT 50",
        )?;

        let rows = stmt.query_map(rusqlite::params![like], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                model: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                message_count: row.get::<_, Option<u32>>(5)?.unwrap_or(0),
            })
        })?;

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Delete a session and all of its messages.
    pub fn delete_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "DELETE FROM messages WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        self.conn.execute(
            "DELETE FROM sessions WHERE id = ?1",
            rusqlite::params![session_id],
        )?;
        Ok(())
    }
}

/// Summary row returned by `list_sessions` and `search_sessions`.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub model: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_sessions_matches_titles_and_message_content() {
        let dir = tempfile::tempdir().expect("temp directory");
        let store =
            SqliteSessionStore::open(&dir.path().join("sessions.db")).expect("open sqlite store");

        store
            .save_session("title-match", Some("Authentication refactor"), "model-a")
            .expect("save title session");
        store
            .save_message("title-match", "title-msg", "user", "unrelated text", None)
            .expect("save title message");

        store
            .save_session("content-match", Some("Build notes"), "model-b")
            .expect("save content session");
        store
            .save_message(
                "content-match",
                "content-msg",
                "assistant",
                "The OAuth callback needs a regression test",
                None,
            )
            .expect("save content message");

        let title_results = store
            .search_sessions("AUTHENTICATION")
            .expect("title search");
        assert_eq!(title_results.len(), 1);
        assert_eq!(title_results[0].id, "title-match");
        assert_eq!(title_results[0].message_count, 1);

        let content_results = store
            .search_sessions("oauth callback")
            .expect("content search");
        assert_eq!(content_results.len(), 1);
        assert_eq!(content_results[0].id, "content-match");
    }

    #[test]
    fn search_sessions_returns_recent_matches_first_and_empty_for_misses() {
        let dir = tempfile::tempdir().expect("temp directory");
        let store =
            SqliteSessionStore::open(&dir.path().join("sessions.db")).expect("open sqlite store");

        for (id, title) in [
            ("older", "Shared routing work"),
            ("newer", "Shared routing tests"),
        ] {
            store
                .save_session(id, Some(title), "model")
                .expect("save session");
            store
                .save_message(id, &format!("{id}-message"), "user", "same keyword", None)
                .expect("save message");
        }
        // `save_session` uses second-resolution timestamps; set deterministic
        // values here so the recency assertion never depends on wall-clock
        // timing or a sleep in the test suite.
        store
            .conn
            .execute(
                "UPDATE sessions SET updated_at = CASE id WHEN 'older' THEN '2026-01-01T00:00:00Z' ELSE '2026-01-02T00:00:00Z' END",
                [],
            )
            .expect("set deterministic timestamps");

        let results = store.search_sessions("shared").expect("search matches");
        assert_eq!(
            results.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            ["newer", "older"]
        );
        assert!(store
            .search_sessions("does-not-exist")
            .expect("empty search")
            .is_empty());
    }
}
