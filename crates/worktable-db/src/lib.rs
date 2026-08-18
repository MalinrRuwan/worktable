use std::sync::Arc;

use anyhow::Context;
use turso_serverless::{Builder, Connection};

const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE IF NOT EXISTS wt_schema_migrations (
        version INTEGER PRIMARY KEY,
        applied_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS wt_ai_sessions (
        id TEXT PRIMARY KEY,
        pi_session_id TEXT NOT NULL UNIQUE,
        title TEXT,
        state TEXT NOT NULL,
        active_run_id TEXT,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS wt_ai_session_items (
        session_id TEXT NOT NULL,
        item_id TEXT NOT NULL,
        relationship TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (session_id, item_id)
    );

    CREATE TABLE IF NOT EXISTS wt_ai_runs (
        id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        request_id TEXT NOT NULL UNIQUE,
        state TEXT NOT NULL,
        started_at INTEGER NOT NULL,
        completed_at INTEGER,
        error TEXT
    );

    CREATE TABLE IF NOT EXISTS wt_ai_events (
        id TEXT PRIMARY KEY,
        run_id TEXT NOT NULL,
        event_type TEXT NOT NULL,
        payload_json TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS wt_ai_session_leases (
        session_id TEXT PRIMARY KEY,
        owner_id TEXT NOT NULL,
        run_id TEXT NOT NULL,
        lease_until INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS wt_pi_fs_entries (
        path TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK (kind IN ('file', 'directory')),
        content TEXT,
        mtime_ms INTEGER NOT NULL
    );
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS wt_entries (
        id TEXT PRIMARY KEY,
        kind TEXT NOT NULL CHECK (kind IN ('text', 'link', 'image')),
        content TEXT NOT NULL,
        title TEXT,
        source TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS wt_entries_created_at_idx
        ON wt_entries (created_at DESC);
    "#,
];

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub title: Option<String>,
    pub source: String,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct TursoStore {
    connection: Arc<Connection>,
    database_url: String,
    auth_token: String,
}

impl TursoStore {
    pub async fn connect(url: &str, auth_token: &str) -> anyhow::Result<Self> {
        let database = Builder::new_remote(url.to_owned())
            .with_auth_token(auth_token.to_owned())
            .build()
            .await
            .context("failed to connect to Turso")?;

        let connection = database
            .connect()
            .context("failed to create Turso connection")?;

        Ok(Self {
            connection: Arc::new(connection),
            database_url: url.to_owned(),
            auth_token: auth_token.to_owned(),
        })
    }

    pub async fn migrate(&self) -> anyhow::Result<()> {
        self.connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS wt_schema_migrations (
                    version INTEGER PRIMARY KEY,
                    applied_at TEXT NOT NULL
                )",
            )
            .await
            .context("failed to initialize Turso migration tracking")?;

        for (index, migration) in MIGRATIONS.iter().enumerate() {
            let version = (index + 1) as i64;

            let already_applied = self
                .connection
                .query(
                    "SELECT 1 FROM wt_schema_migrations WHERE version = ?",
                    [version],
                )
                .await
                .context("failed to inspect Turso migrations")?
                .next()
                .await
                .context("failed to read Turso migration state")?
                .is_some();

            if already_applied {
                continue;
            }

            self.connection
                .execute_batch(migration)
                .await
                .with_context(|| format!("failed to apply Turso migration {version}"))?;

            self.connection
                .execute(
                    "INSERT OR IGNORE INTO wt_schema_migrations (version, applied_at)
                     VALUES (?, datetime('now'))",
                    [version],
                )
                .await
                .with_context(|| format!("failed to record Turso migration {version}"))?;
        }

        Ok(())
    }

    pub async fn list_entries(&self, limit: usize) -> anyhow::Result<Vec<Entry>> {
        let limit = limit.clamp(1, 500) as i64;
        let mut rows = self
            .connection
            .query(
                "SELECT id, kind, content, title, source, created_at
                 FROM wt_entries
                 ORDER BY created_at DESC
                 LIMIT ?",
                [limit],
            )
            .await
            .context("failed to list Worktable entries")?;
        let mut entries = Vec::new();

        while let Some(row) = rows
            .next()
            .await
            .context("failed to read a Worktable entry")?
        {
            entries.push(Entry {
                id: row.get(0).context("invalid entry id")?,
                kind: row.get(1).context("invalid entry kind")?,
                content: row.get(2).context("invalid entry content")?,
                title: row.get(3).context("invalid entry title")?,
                source: row.get(4).context("invalid entry source")?,
                created_at: row.get(5).context("invalid entry timestamp")?,
            });
        }

        Ok(entries)
    }

    pub async fn insert_entry(&self, entry: &Entry) -> anyhow::Result<()> {
        self.connection
            .execute(
                "INSERT INTO wt_entries
                    (id, kind, content, title, source, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
                turso_serverless::params![
                    entry.id.clone(),
                    entry.kind.clone(),
                    entry.content.clone(),
                    entry.title.clone(),
                    entry.source.clone(),
                    entry.created_at
                ],
            )
            .await
            .context("failed to insert Worktable entry")?;

        Ok(())
    }

    pub async fn ensure_session(
        &self,
        session_id: &str,
        title: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        self.connection
            .execute(
                "INSERT INTO wt_ai_sessions
                    (id, pi_session_id, title, state, created_at, updated_at)
                 VALUES (?, ?, ?, 'idle', ?, ?)
                 ON CONFLICT(pi_session_id) DO UPDATE SET
                    title = COALESCE(excluded.title, wt_ai_sessions.title),
                    updated_at = excluded.updated_at",
                turso_serverless::params![session_id, session_id, title, now_ms, now_ms],
            )
            .await
            .context("failed to ensure AI session")?;

        Ok(())
    }

    pub async fn begin_run(
        &self,
        run_id: &str,
        session_id: &str,
        request_id: &str,
        now_ms: i64,
    ) -> anyhow::Result<bool> {
        let affected = self
            .connection
            .execute(
                "INSERT OR IGNORE INTO wt_ai_runs
                    (id, session_id, request_id, state, started_at)
                 VALUES (?, ?, ?, 'running', ?)",
                turso_serverless::params![run_id, session_id, request_id, now_ms],
            )
            .await
            .context("failed to begin AI run")?;

        if affected == 1 {
            self.set_session_state(session_id, "running", Some(run_id), now_ms)
                .await?;
        }

        Ok(affected == 1)
    }

    pub async fn claim_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
        now_ms: i64,
        lease_until_ms: i64,
    ) -> anyhow::Result<bool> {
        let affected = self
            .connection
            .execute(
                "INSERT INTO wt_ai_session_leases
                    (session_id, owner_id, run_id, lease_until, updated_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(session_id) DO UPDATE SET
                    owner_id = excluded.owner_id,
                    run_id = excluded.run_id,
                    lease_until = excluded.lease_until,
                    updated_at = excluded.updated_at
                 WHERE wt_ai_session_leases.lease_until <= excluded.updated_at
                    OR (wt_ai_session_leases.owner_id = excluded.owner_id
                        AND wt_ai_session_leases.run_id = excluded.run_id)",
                turso_serverless::params![session_id, owner_id, run_id, lease_until_ms, now_ms],
            )
            .await
            .context("failed to claim AI session lease")?;

        Ok(affected == 1)
    }

    pub async fn release_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
    ) -> anyhow::Result<bool> {
        let affected = self
            .connection
            .execute(
                "DELETE FROM wt_ai_session_leases
                 WHERE session_id = ? AND owner_id = ? AND run_id = ?",
                turso_serverless::params![session_id, owner_id, run_id],
            )
            .await
            .context("failed to release AI session lease")?;

        Ok(affected == 1)
    }

    pub async fn renew_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
        now_ms: i64,
        lease_until_ms: i64,
    ) -> anyhow::Result<bool> {
        let affected = self
            .connection
            .execute(
                "UPDATE wt_ai_session_leases
                 SET lease_until = ?, updated_at = ?
                 WHERE session_id = ? AND owner_id = ? AND run_id = ?",
                turso_serverless::params![lease_until_ms, now_ms, session_id, owner_id, run_id],
            )
            .await
            .context("failed to renew AI session lease")?;

        Ok(affected == 1)
    }

    pub async fn record_event(
        &self,
        event_id: &str,
        run_id: &str,
        event_type: &str,
        payload_json: &str,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        self.connection
            .execute(
                "INSERT INTO wt_ai_events
                    (id, run_id, event_type, payload_json, created_at)
                 VALUES (?, ?, ?, ?, ?)",
                turso_serverless::params![event_id, run_id, event_type, payload_json, now_ms],
            )
            .await
            .context("failed to record AI event")?;

        Ok(())
    }

    pub async fn finish_run(
        &self,
        run_id: &str,
        session_id: &str,
        state: &str,
        error: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        self.connection
            .execute(
                "UPDATE wt_ai_runs
                 SET state = ?, completed_at = ?, error = ?
                 WHERE id = ?",
                turso_serverless::params![state, now_ms, error, run_id],
            )
            .await
            .context("failed to finish AI run")?;

        self.connection
            .execute(
                "UPDATE wt_ai_sessions
                 SET state = 'idle', active_run_id = NULL, updated_at = ?
                 WHERE pi_session_id = ? AND active_run_id = ?",
                turso_serverless::params![now_ms, session_id, run_id],
            )
            .await
            .context("failed to finish AI session")?;

        Ok(())
    }

    pub fn connection(&self) -> Arc<Connection> {
        Arc::clone(&self.connection)
    }

    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    pub fn auth_token(&self) -> &str {
        &self.auth_token
    }

    async fn set_session_state(
        &self,
        session_id: &str,
        state: &str,
        active_run_id: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        self.connection
            .execute(
                "UPDATE wt_ai_sessions
                 SET state = ?, active_run_id = ?, updated_at = ?
                 WHERE pi_session_id = ?",
                turso_serverless::params![state, active_run_id, now_ms, session_id],
            )
            .await
            .context("failed to update AI session state")?;

        Ok(())
    }
}
