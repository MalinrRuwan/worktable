use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[cfg(not(target_arch = "wasm32"))]
use rusqlite::{Connection, OptionalExtension, params};

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
        content TEXT NOT NULL,
        title TEXT,
        source TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS wt_entries_created_at_idx
        ON wt_entries (created_at DESC);
    "#,
    r#"
    CREATE TABLE IF NOT EXISTS wt_ai_provider_credentials (
        provider_id TEXT PRIMARY KEY,
        credential_json TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS wt_ai_config (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL,
        updated_at INTEGER NOT NULL
    );
    "#,
];

/// Chat storage: the picker's transcripts and the model context behind them.
///
/// Created from `migrate` on every launch rather than from [`MIGRATIONS`],
/// because a database can already carry a higher migration number than this
/// array has entries (an experiment that was later removed leaves its version
/// recorded). A versioned entry would then be skipped and leave the tables
/// missing, which surfaces as "no such table: wt_ai_history" at the first
/// prompt. `IF NOT EXISTS` makes the repair idempotent.
const CHAT_SCHEMA: &str = r#"
    CREATE TABLE IF NOT EXISTS wt_chats (
        id TEXT PRIMARY KEY,
        title TEXT NOT NULL,
        messages_json TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        revision INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS wt_chats_updated_at_idx
        ON wt_chats (updated_at DESC);

    CREATE TABLE IF NOT EXISTS wt_ai_history (
        session_id TEXT PRIMARY KEY,
        messages_json TEXT NOT NULL
    );
"#;

/// Metadata for the chat picker; transcripts are loaded only on selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct ChatSummary {
    pub id: String,
    pub title: String,
    pub updated_at: i64,
    pub revision: i64,
}

impl ChatSummary {
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        updated_at: i64,
        revision: i64,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            updated_at,
            revision,
        }
    }
}

/// A versioned UI transcript, separate from the model's tool-call history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct StoredChat {
    pub summary: ChatSummary,
    pub messages_json: String,
}

impl StoredChat {
    pub fn new(summary: ChatSummary, messages_json: String) -> Self {
        Self {
            summary,
            messages_json,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub content: String,
    pub title: Option<String>,
    pub source: String,
    pub created_at: i64,
}

/// A stored AI provider credential (serialized into `wt_ai_provider_credentials`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredential {
    pub kind: String,
    pub key: String,
}

// ---------------------------------------------------------------------------
// Native (non-wasm) SQLite store — rusqlite + WAL
// ---------------------------------------------------------------------------
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
    database_path: String,
}

#[cfg(not(target_arch = "wasm32"))]
impl SqliteStore {
    /// Open (or create) the local SQLite datafile.
    pub fn connect(path: &str) -> anyhow::Result<Self> {
        let connection = Connection::open(path).context("failed to open local SQLite database")?;
        // WAL mode keeps concurrent reads reliable and avoids read/write lockups
        // during AI requests.
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("failed to enable WAL mode")?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .context("failed to configure synchronous mode")?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .context("failed to set busy timeout")?;

        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
            database_path: path.to_owned(),
        })
    }

    pub fn database_path(&self) -> &str {
        &self.database_path
    }

    pub fn list_chats(&self) -> anyhow::Result<Vec<ChatSummary>> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let mut statement = connection.prepare(
            "SELECT id, title, updated_at, revision FROM wt_chats ORDER BY updated_at DESC, id",
        )?;
        let chats = statement.query_map([], |row| {
            Ok(ChatSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                updated_at: row.get(2)?,
                revision: row.get(3)?,
            })
        })?;
        Ok(chats.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn load_chat(&self, id: &str) -> anyhow::Result<Option<StoredChat>> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        Ok(connection
            .query_row(
                "SELECT id, title, updated_at, revision, messages_json FROM wt_chats WHERE id = ?",
                [id],
                |row| {
                    Ok(StoredChat {
                        summary: ChatSummary {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            updated_at: row.get(2)?,
                            revision: row.get(3)?,
                        },
                        messages_json: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn save_chat(&self, chat: &StoredChat) -> anyhow::Result<()> {
        serde_json::from_str::<serde_json::Value>(&chat.messages_json)
            .context("invalid chat transcript")?;
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection.execute(
            "INSERT INTO wt_chats (id, title, updated_at, revision, messages_json)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 title = excluded.title, updated_at = excluded.updated_at,
                 revision = excluded.revision, messages_json = excluded.messages_json
             WHERE excluded.revision >= wt_chats.revision",
            params![
                chat.summary.id,
                chat.summary.title,
                chat.summary.updated_at,
                chat.summary.revision,
                chat.messages_json
            ],
        )?;
        Ok(())
    }

    pub fn load_session_history(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        Ok(connection
            .query_row(
                "SELECT messages_json FROM wt_ai_history WHERE session_id = ?",
                [session_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_session_history(
        &self,
        session_id: &str,
        messages_json: &str,
    ) -> anyhow::Result<()> {
        serde_json::from_str::<serde_json::Value>(messages_json)
            .context("invalid assistant history")?;
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection.execute(
            "INSERT INTO wt_ai_history (session_id, messages_json) VALUES (?, ?)
             ON CONFLICT(session_id) DO UPDATE SET messages_json = excluded.messages_json",
            params![session_id, messages_json],
        )?;
        Ok(())
    }

    pub fn migrate(&self) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS wt_schema_migrations (
                    version INTEGER PRIMARY KEY,
                    applied_at TEXT NOT NULL
                )",
            )
            .context("failed to initialize migration tracking")?;

        for (index, migration) in MIGRATIONS.iter().enumerate() {
            let version = (index + 1) as i64;

            let already_applied = connection
                .query_row(
                    "SELECT 1 FROM wt_schema_migrations WHERE version = ?",
                    [version],
                    |_| Ok(()),
                )
                .is_ok();

            if already_applied {
                continue;
            }

            connection
                .execute_batch(migration)
                .with_context(|| format!("failed to apply migration {version}"))?;

            connection
                .execute(
                    "INSERT OR IGNORE INTO wt_schema_migrations (version, applied_at)
                     VALUES (?, datetime('now'))",
                    [version],
                )
                .with_context(|| format!("failed to record migration {version}"))?;
        }

        // The entry category was removed from the product; drop the legacy
        // column so old databases match the new schema.
        let has_kind = connection
            .prepare("SELECT 1 FROM pragma_table_info('wt_entries') WHERE name = 'kind'")
            .and_then(|mut statement| statement.exists([]))
            .unwrap_or(false);
        if has_kind {
            connection
                .execute("ALTER TABLE wt_entries DROP COLUMN kind", [])
                .context("failed to drop the legacy entry kind column")?;
        }

        // Chat storage is repaired on every launch, not versioned — see
        // [`CHAT_SCHEMA`].
        connection
            .execute_batch(CHAT_SCHEMA)
            .context("failed to ensure the chat schema")?;

        Ok(())
    }

    pub fn list_entries(&self, limit: usize) -> anyhow::Result<Vec<Entry>> {
        let limit = limit.clamp(1, 500) as i64;
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let mut statement = connection
            .prepare(
                "SELECT id, content, title, source, created_at
                 FROM wt_entries
                 ORDER BY created_at DESC
                 LIMIT ?",
            )
            .context("failed to prepare entry list")?;
        let mut rows = statement
            .query([limit])
            .context("failed to query Worktable entries")?;
        let mut entries = Vec::new();

        while let Some(row) = rows.next().context("failed to read a Worktable entry")? {
            entries.push(Entry {
                id: row.get(0).context("invalid entry id")?,
                content: row.get(1).context("invalid entry content")?,
                title: row.get(2).context("invalid entry title")?,
                source: row.get(3).context("invalid entry source")?,
                created_at: row.get(4).context("invalid entry timestamp")?,
            });
        }

        Ok(entries)
    }

    pub fn insert_entry(&self, entry: &Entry) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "INSERT INTO wt_entries
                    (id, content, title, source, created_at)
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    entry.id.clone(),
                    entry.content.clone(),
                    entry.title.clone(),
                    entry.source.clone(),
                    entry.created_at
                ],
            )
            .context("failed to insert Worktable entry")?;

        Ok(())
    }

    pub fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute("DELETE FROM wt_entries WHERE id = ?", [id])
            .context("failed to delete Worktable entry")?;

        Ok(())
    }

    /// Replace an entry's content (the detail editor's Save).
    pub fn update_entry_content(&self, id: &str, content: &str) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "UPDATE wt_entries SET content = ? WHERE id = ?",
                params![content, id],
            )
            .context("failed to update Worktable entry")?;

        Ok(())
    }

    pub fn ensure_session(
        &self,
        session_id: &str,
        title: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "INSERT INTO wt_ai_sessions
                    (id, pi_session_id, title, state, created_at, updated_at)
                 VALUES (?, ?, ?, 'idle', ?, ?)
                 ON CONFLICT(pi_session_id) DO UPDATE SET
                    title = COALESCE(excluded.title, wt_ai_sessions.title),
                    updated_at = excluded.updated_at",
                params![session_id, session_id, title, now_ms, now_ms],
            )
            .context("failed to ensure AI session")?;

        Ok(())
    }

    pub fn begin_run(
        &self,
        run_id: &str,
        session_id: &str,
        request_id: &str,
        now_ms: i64,
    ) -> anyhow::Result<bool> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let affected = connection
            .execute(
                "INSERT OR IGNORE INTO wt_ai_runs
                    (id, session_id, request_id, state, started_at)
                 VALUES (?, ?, ?, 'running', ?)",
                params![run_id, session_id, request_id, now_ms],
            )
            .context("failed to begin AI run")?;

        if affected == 1 {
            // Update the session state with the lock we already hold — calling
            // a helper that re-acquires `self.connection` would deadlock
            // (std Mutex is not reentrant).
            connection
                .execute(
                    "UPDATE wt_ai_sessions
                     SET state = 'running', active_run_id = ?, updated_at = ?
                     WHERE pi_session_id = ?",
                    params![run_id, now_ms, session_id],
                )
                .context("failed to mark AI session running")?;
        }

        Ok(affected == 1)
    }

    pub fn claim_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
        now_ms: i64,
        lease_until_ms: i64,
    ) -> anyhow::Result<bool> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let affected = connection
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
                params![session_id, owner_id, run_id, lease_until_ms, now_ms],
            )
            .context("failed to claim AI session lease")?;

        Ok(affected == 1)
    }

    pub fn release_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
    ) -> anyhow::Result<bool> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let affected = connection
            .execute(
                "DELETE FROM wt_ai_session_leases
                 WHERE session_id = ? AND owner_id = ? AND run_id = ?",
                params![session_id, owner_id, run_id],
            )
            .context("failed to release AI session lease")?;

        Ok(affected == 1)
    }

    pub fn renew_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
        now_ms: i64,
        lease_until_ms: i64,
    ) -> anyhow::Result<bool> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let affected = connection
            .execute(
                "UPDATE wt_ai_session_leases
                 SET lease_until = ?, updated_at = ?
                 WHERE session_id = ? AND owner_id = ? AND run_id = ?",
                params![lease_until_ms, now_ms, session_id, owner_id, run_id],
            )
            .context("failed to renew AI session lease")?;

        Ok(affected == 1)
    }

    pub fn record_event(
        &self,
        event_id: &str,
        run_id: &str,
        event_type: &str,
        payload_json: &str,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "INSERT INTO wt_ai_events
                    (id, run_id, event_type, payload_json, created_at)
                 VALUES (?, ?, ?, ?, ?)",
                params![event_id, run_id, event_type, payload_json, now_ms],
            )
            .context("failed to record AI event")?;

        Ok(())
    }

    /// Mark every `running` run as `failed` ("interrupted by restart").
    ///
    /// Runs whose app died mid-prompt (force-quit, crash) stay `running`
    /// forever otherwise — they pile up row after row and misreport activity.
    /// Called once at AI-worker startup: at that point no run can legitimately
    /// be `running`, because runs only progress while this process lives.
    pub fn fail_stale_runs(&self, now_ms: i64) -> anyhow::Result<usize> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let affected = connection
            .execute(
                "UPDATE wt_ai_runs
                 SET state = 'failed', completed_at = ?, error = 'interrupted by restart'
                 WHERE state = 'running'",
                params![now_ms],
            )
            .context("failed to fail stale AI runs")?;
        Ok(affected)
    }

    /// Current state of a run row (`running` / `completed` / `failed`), or
    /// `None` when the run id is unknown.
    pub fn run_state(&self, run_id: &str) -> anyhow::Result<Option<String>> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let mut statement = connection
            .prepare("SELECT state FROM wt_ai_runs WHERE id = ?")
            .context("failed to query run state")?;
        let mut rows = statement.query(params![run_id])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        session_id: &str,
        state: &str,
        error: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "UPDATE wt_ai_runs
                 SET state = ?, completed_at = ?, error = ?
                 WHERE id = ?",
                params![state, now_ms, error, run_id],
            )
            .context("failed to finish AI run")?;

        connection
            .execute(
                "UPDATE wt_ai_sessions
                 SET state = 'idle', active_run_id = NULL, updated_at = ?
                 WHERE pi_session_id = ? AND active_run_id = ?",
                params![now_ms, session_id, run_id],
            )
            .context("failed to finish AI session")?;

        Ok(())
    }

    // ---- AI provider credentials + active selection (shared with the AI runtime) ----

    /// Read a stored provider credential (e.g. an API key).
    pub fn read_provider_credential(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<ProviderCredential>> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let credential_json: Option<String> = connection
            .query_row(
                "SELECT credential_json FROM wt_ai_provider_credentials WHERE provider_id = ?",
                [provider_id],
                |row| row.get(0),
            )
            .optional()
            .context("failed to read provider credential")?;
        let Some(credential_json) = credential_json else {
            return Ok(None);
        };
        Ok(serde_json::from_str(&credential_json).unwrap_or(None))
    }

    /// Read every stored credential as a provider_id -> credential map.
    pub fn list_provider_credentials(
        &self,
    ) -> anyhow::Result<std::collections::HashMap<String, ProviderCredential>> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let mut statement = connection
            .prepare("SELECT provider_id, credential_json FROM wt_ai_provider_credentials")
            .context("failed to prepare credential list")?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .context("failed to query provider credentials")?;
        let mut credentials = std::collections::HashMap::new();
        for row in rows {
            let (provider_id, credential_json) = row.context("failed to read credential row")?;
            if let Ok(Some(credential)) =
                serde_json::from_str::<Option<ProviderCredential>>(&credential_json)
            {
                credentials.insert(provider_id, credential);
            }
        }
        Ok(credentials)
    }

    /// Upsert a provider credential.
    pub fn write_provider_credential(
        &self,
        provider_id: &str,
        credential: &ProviderCredential,
    ) -> anyhow::Result<()> {
        let credential_json = serde_json::to_string(&Some(credential))?;
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "INSERT INTO wt_ai_provider_credentials (provider_id, credential_json, updated_at)
                 VALUES (?, ?, ?)
                 ON CONFLICT(provider_id) DO UPDATE SET
                   credential_json = excluded.credential_json,
                   updated_at = excluded.updated_at",
                params![provider_id, credential_json, now_ms()],
            )
            .context("failed to write provider credential")?;
        Ok(())
    }

    /// Remove a stored provider credential.
    pub fn delete_provider_credential(&self, provider_id: &str) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "DELETE FROM wt_ai_provider_credentials WHERE provider_id = ?",
                [provider_id],
            )
            .context("failed to delete provider credential")?;
        Ok(())
    }

    /// Read a `wt_ai_config` value (e.g. `active_provider`, `active_model`).
    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM wt_ai_config WHERE key = ?",
                [key],
                |row| row.get(0),
            )
            .optional()
            .context("failed to read AI config")?;
        Ok(value)
    }

    /// Upsert a `wt_ai_config` value (e.g. `active_provider`, `active_model`).
    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "INSERT INTO wt_ai_config (key, value, updated_at)
                 VALUES (?, ?, ?)
                 ON CONFLICT(key) DO UPDATE SET
                   value = excluded.value,
                   updated_at = excluded.updated_at",
                params![key, value, now_ms()],
            )
            .context("failed to write AI config")?;
        Ok(())
    }

    /// Delete a `wt_ai_config` value.
    pub fn delete_config(&self, key: &str) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute("DELETE FROM wt_ai_config WHERE key = ?", [key])
            .context("failed to delete AI config")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// WASM store — in-memory + localStorage fallback
// ---------------------------------------------------------------------------
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub struct SqliteStore {
    inner: Arc<Mutex<WasmInner>>,
    database_path: String,
}

#[cfg(target_arch = "wasm32")]
#[derive(Default)]
struct WasmInner {
    entries: Vec<Entry>,
    credentials: std::collections::HashMap<String, ProviderCredential>,
    config: std::collections::HashMap<String, String>,
    chats: std::collections::HashMap<String, StoredChat>,
    history: std::collections::HashMap<String, String>,
    sessions: std::collections::HashMap<String, (String, i64)>, // session_id -> (state, updated_at)
    runs: std::collections::HashMap<String, String>,            // run_id -> state
    leases: std::collections::HashMap<String, (String, String, i64)>, // session_id -> (owner_id, run_id, lease_until)
}

#[cfg(target_arch = "wasm32")]
impl SqliteStore {
    pub fn connect(path: &str) -> anyhow::Result<Self> {
        let store = Self {
            inner: Arc::new(Mutex::new(WasmInner::default())),
            database_path: path.to_owned(),
        };
        // Try to hydrate from localStorage (best-effort, ignore errors)
        store.load_from_storage();
        Ok(store)
    }

    pub fn database_path(&self) -> &str {
        &self.database_path
    }

    pub fn list_chats(&self) -> anyhow::Result<Vec<ChatSummary>> {
        let inner = self.inner.lock().expect("wasm lock poisoned");
        let mut chats: Vec<_> = inner
            .chats
            .values()
            .map(|chat| chat.summary.clone())
            .collect();
        chats.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(chats)
    }

    pub fn load_chat(&self, id: &str) -> anyhow::Result<Option<StoredChat>> {
        Ok(self
            .inner
            .lock()
            .expect("wasm lock poisoned")
            .chats
            .get(id)
            .cloned())
    }

    pub fn save_chat(&self, chat: &StoredChat) -> anyhow::Result<()> {
        serde_json::from_str::<serde_json::Value>(&chat.messages_json)
            .context("invalid chat transcript")?;
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        if inner
            .chats
            .get(&chat.summary.id)
            .is_none_or(|existing| chat.summary.revision >= existing.summary.revision)
        {
            inner.chats.insert(chat.summary.id.clone(), chat.clone());
        }
        drop(inner);
        self.save_to_storage();
        Ok(())
    }

    pub fn load_session_history(&self, session_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self
            .inner
            .lock()
            .expect("wasm lock poisoned")
            .history
            .get(session_id)
            .cloned())
    }

    pub fn save_session_history(
        &self,
        session_id: &str,
        messages_json: &str,
    ) -> anyhow::Result<()> {
        serde_json::from_str::<serde_json::Value>(messages_json)
            .context("invalid assistant history")?;
        self.inner
            .lock()
            .expect("wasm lock poisoned")
            .history
            .insert(session_id.to_owned(), messages_json.to_owned());
        self.save_to_storage();
        Ok(())
    }

    pub fn migrate(&self) -> anyhow::Result<()> {
        // No-op for in-memory; ensure storage key exists
        Ok(())
    }

    pub fn list_entries(&self, limit: usize) -> anyhow::Result<Vec<Entry>> {
        let inner = self.inner.lock().expect("wasm lock poisoned");
        let mut entries = inner.entries.clone();
        entries.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        entries.truncate(limit.clamp(1, 500));
        Ok(entries)
    }

    pub fn insert_entry(&self, entry: &Entry) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        // Upsert by id
        if let Some(pos) = inner.entries.iter().position(|e| e.id == entry.id) {
            inner.entries[pos] = entry.clone();
        } else {
            inner.entries.push(entry.clone());
        }
        drop(inner);
        self.save_to_storage();
        Ok(())
    }

    pub fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        inner.entries.retain(|e| e.id != id);
        drop(inner);
        self.save_to_storage();
        Ok(())
    }

    /// Replace an entry's content (the detail editor's Save).
    pub fn update_entry_content(&self, id: &str, content: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        if let Some(entry) = inner.entries.iter_mut().find(|entry| entry.id == id) {
            entry.content = content.to_owned();
        }
        drop(inner);
        self.save_to_storage();
        Ok(())
    }

    pub fn ensure_session(
        &self,
        session_id: &str,
        _title: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        inner
            .sessions
            .entry(session_id.to_owned())
            .or_insert_with(|| ("idle".to_owned(), now_ms));
        Ok(())
    }

    pub fn begin_run(
        &self,
        run_id: &str,
        session_id: &str,
        _request_id: &str,
        now_ms: i64,
    ) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        if inner.runs.contains_key(run_id) {
            return Ok(false);
        }
        inner.runs.insert(run_id.to_owned(), "running".to_owned());
        if let Some(entry) = inner.sessions.get_mut(session_id) {
            entry.0 = "running".to_owned();
            entry.1 = now_ms;
        }
        Ok(true)
    }

    pub fn claim_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
        now_ms: i64,
        lease_until_ms: i64,
    ) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        let should_claim = match inner.leases.get(session_id) {
            None => true,
            Some((_, _, until)) => *until <= now_ms,
        };
        if should_claim {
            inner.leases.insert(
                session_id.to_owned(),
                (owner_id.to_owned(), run_id.to_owned(), lease_until_ms),
            );
            Ok(true)
        } else {
            // Check if same owner/run can renew
            if let Some((oid, rid, _)) = inner.leases.get(session_id) {
                if oid == owner_id && rid == run_id {
                    inner.leases.insert(
                        session_id.to_owned(),
                        (owner_id.to_owned(), run_id.to_owned(), lease_until_ms),
                    );
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }

    pub fn release_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
    ) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        if let Some((oid, rid, _)) = inner.leases.get(session_id) {
            if oid == owner_id && rid == run_id {
                inner.leases.remove(session_id);
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn renew_session_lease(
        &self,
        session_id: &str,
        owner_id: &str,
        run_id: &str,
        _now_ms: i64,
        lease_until_ms: i64,
    ) -> anyhow::Result<bool> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        if let Some((oid, rid, _)) = inner.leases.get_mut(session_id) {
            if oid == owner_id && rid == run_id {
                *inner.leases.get_mut(session_id).unwrap() =
                    (owner_id.to_owned(), run_id.to_owned(), lease_until_ms);
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn record_event(
        &self,
        _event_id: &str,
        _run_id: &str,
        _event_type: &str,
        _payload_json: &str,
        _now_ms: i64,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn finish_run(
        &self,
        run_id: &str,
        session_id: &str,
        state: &str,
        _error: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        inner.runs.insert(run_id.to_owned(), state.to_owned());
        if let Some(entry) = inner.sessions.get_mut(session_id) {
            entry.0 = "idle".to_owned();
            entry.1 = now_ms;
        }
        Ok(())
    }

    pub fn read_provider_credential(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<ProviderCredential>> {
        let inner = self.inner.lock().expect("wasm lock poisoned");
        Ok(inner.credentials.get(provider_id).cloned())
    }

    pub fn list_provider_credentials(
        &self,
    ) -> anyhow::Result<std::collections::HashMap<String, ProviderCredential>> {
        let inner = self.inner.lock().expect("wasm lock poisoned");
        Ok(inner.credentials.clone())
    }

    pub fn write_provider_credential(
        &self,
        provider_id: &str,
        credential: &ProviderCredential,
    ) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        inner
            .credentials
            .insert(provider_id.to_owned(), credential.clone());
        drop(inner);
        self.save_to_storage();
        Ok(())
    }

    pub fn delete_provider_credential(&self, provider_id: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        inner.credentials.remove(provider_id);
        drop(inner);
        self.save_to_storage();
        Ok(())
    }

    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        let inner = self.inner.lock().expect("wasm lock poisoned");
        Ok(inner.config.get(key).cloned())
    }

    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        inner.config.insert(key.to_owned(), value.to_owned());
        drop(inner);
        self.save_to_storage();
        Ok(())
    }

    pub fn delete_config(&self, key: &str) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().expect("wasm lock poisoned");
        inner.config.remove(key);
        drop(inner);
        self.save_to_storage();
        Ok(())
    }

    fn load_from_storage(&self) {
        // Best-effort hydrate from localStorage (key: "worktable-db")
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(window) = web_sys::window() {
                if let Ok(Some(storage)) = window.local_storage() {
                    if let Ok(Some(json)) = storage.get_item("worktable-db") {
                        if let Ok(saved) = serde_json::from_str::<serde_json::Value>(&json) {
                            let mut inner = self.inner.lock().expect("wasm lock poisoned");
                            if let Some(entries) = saved
                                .get("entries")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                            {
                                inner.entries = entries;
                            }
                            if let Some(creds) = saved
                                .get("credentials")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                            {
                                inner.credentials = creds;
                            }
                            if let Some(cfg) = saved
                                .get("config")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                            {
                                inner.config = cfg;
                            }
                            if let Some(chats) = saved
                                .get("chats")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                            {
                                inner.chats = chats;
                            }
                            if let Some(history) = saved
                                .get("history")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                            {
                                inner.history = history;
                            }
                        }
                    }
                }
            }
        }
    }

    fn save_to_storage(&self) {
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(window) = web_sys::window() {
                if let Ok(Some(storage)) = window.local_storage() {
                    let inner = self.inner.lock().expect("wasm lock poisoned");
                    let payload = serde_json::json!({
                        "entries": inner.entries,
                        "credentials": inner.credentials,
                        "config": inner.config,
                        "chats": inner.chats,
                        "history": inner.history,
                    });
                    if let Ok(json) = serde_json::to_string(&payload) {
                        let _ = storage.set_item("worktable-db", &json);
                    }
                }
            }
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh in-memory store (single shared connection, so `:memory:` works).
    fn store() -> SqliteStore {
        let store = SqliteStore::connect(":memory:").expect("connect in-memory");
        store.migrate().expect("migrate");
        store
    }

    fn entry(id: &str, created_at: i64) -> Entry {
        Entry {
            id: id.to_owned(),
            content: format!("content of {id}"),
            title: None,
            source: "Worktable".to_owned(),
            created_at,
        }
    }

    #[test]
    fn entries_insert_list_delete() {
        let store = store();
        store.insert_entry(&entry("a", 100)).unwrap();
        store.insert_entry(&entry("b", 300)).unwrap();
        store.insert_entry(&entry("c", 200)).unwrap();

        let listed = store.list_entries(10).unwrap();
        let ids: Vec<&str> = listed.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c", "a"], "entries list recency-desc");

        store.delete_entry("b").unwrap();
        let ids: Vec<String> = store
            .list_entries(10)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids, vec!["c", "a"]);

        let limited = store.list_entries(1).unwrap();
        assert_eq!(limited.len(), 1, "limit is honored");
    }

    #[test]
    fn session_lease_single_holder_until_release_or_expiry() {
        let store = store();
        store.ensure_session("s", None, 1_000).unwrap();

        // First claim wins.
        assert!(
            store
                .claim_session_lease("s", "owner-1", "run-1", 1_000, 31_000)
                .unwrap()
        );
        // A different owner/run cannot claim while the lease is live.
        assert!(
            !store
                .claim_session_lease("s", "owner-2", "run-2", 2_000, 32_000)
                .unwrap()
        );
        // Even the same owner with a *different* run cannot steal it.
        assert!(
            !store
                .claim_session_lease("s", "owner-1", "run-9", 2_000, 32_000)
                .unwrap()
        );
        // The holder can re-claim (idempotent renew-style claim).
        assert!(
            store
                .claim_session_lease("s", "owner-1", "run-1", 2_000, 32_000)
                .unwrap()
        );
        // After expiry (lease_until 32_000 <= now 40_000), someone else can claim.
        assert!(
            store
                .claim_session_lease("s", "owner-2", "run-2", 40_000, 70_000)
                .unwrap()
        );
        // Release requires the exact owner+run.
        assert!(
            !store
                .release_session_lease("s", "owner-1", "run-1")
                .unwrap()
        );
        assert!(
            store
                .release_session_lease("s", "owner-2", "run-2")
                .unwrap()
        );
        // Freed: a fresh claim succeeds.
        assert!(
            store
                .claim_session_lease("s", "owner-3", "run-3", 50_000, 80_000)
                .unwrap()
        );
    }

    #[test]
    fn lease_renewal_requires_holder_identity() {
        let store = store();
        store.ensure_session("s", None, 1_000).unwrap();
        assert!(
            store
                .claim_session_lease("s", "owner-1", "run-1", 1_000, 2_000)
                .unwrap()
        );

        assert!(
            store
                .renew_session_lease("s", "owner-1", "run-1", 1_500, 3_000)
                .unwrap()
        );
        assert!(
            !store
                .renew_session_lease("s", "owner-2", "run-1", 1_500, 3_000)
                .unwrap()
        );
        assert!(
            !store
                .renew_session_lease("s", "owner-1", "run-2", 1_500, 3_000)
                .unwrap()
        );
    }

    #[test]
    fn run_lifecycle_begin_finish_is_idempotent_on_ids() {
        let store = store();
        store.ensure_session("s", None, 1_000).unwrap();

        assert!(store.begin_run("run-1", "s", "req-1", 1_000).unwrap());
        // Same run id cannot begin twice (INSERT OR IGNORE).
        assert!(!store.begin_run("run-1", "s", "req-1", 1_100).unwrap());

        store
            .finish_run("run-1", "s", "completed", None, 2_000)
            .unwrap();
        // Finishing twice is a no-op, not an error.
        store
            .finish_run("run-1", "s", "completed", None, 3_000)
            .unwrap();

        // A new run on the same session works after the previous finished.
        assert!(store.begin_run("run-2", "s", "req-2", 4_000).unwrap());
        store
            .finish_run("run-2", "s", "failed", Some("boom"), 5_000)
            .unwrap();
    }

    #[test]
    fn provider_credentials_roundtrip_and_delete() {
        let store = store();
        let credential = ProviderCredential {
            kind: "api_key".to_owned(),
            key: "sk-test".to_owned(),
        };
        store
            .write_provider_credential("openai", &credential)
            .unwrap();

        let read = store.read_provider_credential("openai").unwrap().unwrap();
        assert_eq!(read.key, "sk-test");
        assert_eq!(read.kind, "api_key");

        let listed = store.list_provider_credentials().unwrap();
        assert!(listed.contains_key("openai"));

        store.delete_provider_credential("openai").unwrap();
        assert!(store.read_provider_credential("openai").unwrap().is_none());
    }

    #[test]
    fn config_set_get_delete() {
        let store = store();
        assert_eq!(store.get_config("active_provider").unwrap(), None);
        store.set_config("active_provider", "openai").unwrap();
        assert_eq!(
            store.get_config("active_provider").unwrap().as_deref(),
            Some("openai")
        );
        store.set_config("active_provider", "deepseek").unwrap();
        assert_eq!(
            store.get_config("active_provider").unwrap().as_deref(),
            Some("deepseek")
        );
        store.delete_config("active_provider").unwrap();
        assert_eq!(store.get_config("active_provider").unwrap(), None);
    }

    fn chat(id: &str, updated_at: i64, revision: i64) -> StoredChat {
        StoredChat {
            summary: ChatSummary {
                id: id.into(),
                title: format!("Chat {id}"),
                updated_at,
                revision,
            },
            messages_json: format!(r#"[{{"text":"revision {revision}"}}]"#),
        }
    }

    #[test]
    fn chats_roundtrip_order_and_reject_stale_saves() {
        let store = store();
        assert!(store.list_chats().unwrap().is_empty());
        assert!(store.load_chat("missing").unwrap().is_none());
        store.save_chat(&chat("a", 10, 1)).unwrap();
        store.save_chat(&chat("b", 20, 1)).unwrap();
        store.save_chat(&chat("a", 30, 3)).unwrap();
        store.save_chat(&chat("a", 40, 2)).unwrap();
        assert_eq!(store.load_chat("a").unwrap(), Some(chat("a", 30, 3)));
        assert_eq!(
            store
                .list_chats()
                .unwrap()
                .iter()
                .map(|chat| chat.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        let mut invalid = chat("c", 40, 1);
        invalid.messages_json = "broken JSON".into();
        assert!(store.save_chat(&invalid).is_err());
        assert!(store.load_chat("c").unwrap().is_none());
    }

    /// A database whose migration counter is already ahead of [`MIGRATIONS`]
    /// (an experiment that was later removed leaves its version recorded) must
    /// still get the chat tables. Pinning them to a version would skip them
    /// here and fail the first prompt with "no such table: wt_ai_history".
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn chat_schema_is_repaired_on_a_database_whose_migration_counter_is_ahead() {
        let store = SqliteStore::connect(":memory:").unwrap();
        {
            let connection = store.connection.lock().unwrap();
            for (ix, migration) in MIGRATIONS.iter().enumerate() {
                connection.execute_batch(migration).unwrap();
                connection
                    .execute(
                        "INSERT INTO wt_schema_migrations (version, applied_at)
                         VALUES (?, 'before chats')",
                        [ix as i64 + 1],
                    )
                    .unwrap();
            }
            // The removed experiment: a recorded version with no chat tables.
            connection
                .execute(
                    "INSERT INTO wt_schema_migrations (version, applied_at)
                     VALUES (?, 'removed experiment')",
                    [MIGRATIONS.len() as i64 + 1],
                )
                .unwrap();
            connection
                .execute_batch("CREATE TABLE wt_ai_chats (id TEXT PRIMARY KEY, title TEXT NOT NULL)")
                .unwrap();
        }

        store.migrate().unwrap();
        // Repeatable: a second launch must not fail on the existing tables.
        store.migrate().unwrap();
        store.save_chat(&chat("saved", 10, 1)).unwrap();
        assert_eq!(
            store.load_chat("saved").unwrap(),
            Some(chat("saved", 10, 1))
        );
        store
            .save_session_history("saved", r#"[{"text":"context"}]"#)
            .unwrap();
        assert!(store.load_session_history("saved").unwrap().is_some());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn chats_and_model_context_survive_reopening_the_database() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("worktable-chat-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chats.db").to_string_lossy().into_owned();
        {
            let store = SqliteStore::connect(&path).unwrap();
            store.migrate().unwrap();
            store.save_chat(&chat("a", 10, 1)).unwrap();
            store
                .save_session_history("a", r#"[{"text":"context A"}]"#)
                .unwrap();
            store
                .save_session_history("b", r#"[{"text":"context B"}]"#)
                .unwrap();
        }
        let reopened = SqliteStore::connect(&path).unwrap();
        reopened.migrate().unwrap();
        assert_eq!(reopened.load_chat("a").unwrap(), Some(chat("a", 10, 1)));
        assert!(
            reopened
                .load_session_history("a")
                .unwrap()
                .unwrap()
                .contains("context A")
        );
        assert!(
            reopened
                .load_session_history("b")
                .unwrap()
                .unwrap()
                .contains("context B")
        );
        assert!(reopened.load_session_history("missing").unwrap().is_none());
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
