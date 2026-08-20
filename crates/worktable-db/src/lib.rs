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
        kind TEXT NOT NULL CHECK (kind IN ('text', 'link', 'image')),
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: String,
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

        Ok(())
    }

    pub fn list_entries(&self, limit: usize) -> anyhow::Result<Vec<Entry>> {
        let limit = limit.clamp(1, 500) as i64;
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        let mut statement = connection
            .prepare(
                "SELECT id, kind, content, title, source, created_at
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
                kind: row.get(1).context("invalid entry kind")?,
                content: row.get(2).context("invalid entry content")?,
                title: row.get(3).context("invalid entry title")?,
                source: row.get(4).context("invalid entry source")?,
                created_at: row.get(5).context("invalid entry timestamp")?,
            });
        }

        Ok(entries)
    }

    pub fn insert_entry(&self, entry: &Entry) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "INSERT INTO wt_entries
                    (id, kind, content, title, source, created_at)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    entry.id.clone(),
                    entry.kind.clone(),
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
            self.set_session_state(session_id, "running", Some(run_id), now_ms)?;
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

    fn set_session_state(
        &self,
        session_id: &str,
        state: &str,
        active_run_id: Option<&str>,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        let connection = self.connection.lock().expect("sqlite lock poisoned");
        connection
            .execute(
                "UPDATE wt_ai_sessions
                 SET state = ?, active_run_id = ?, updated_at = ?
                 WHERE pi_session_id = ?",
                params![state, active_run_id, now_ms, session_id],
            )
            .context("failed to update AI session state")?;

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
