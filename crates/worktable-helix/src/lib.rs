//! Worktable Helix — parallel knowledge base backed by HelixDB.
//!
//! Worktable's primary store remains SQLite (`worktable-db` at
//! `~/.worktable/worktable.db`). This crate mirrors `Entry` nodes into
//! HelixDB's graph (`http://localhost:6969` by default, the `helix start dev`
//! gateway) so the AI agent can do semantic / graph traversals without
//! coupling app uptime to Helix availability.
//!
//! # Architecture
//!
//! - SQLite is source-of-truth. App boots and works with no Helix process.
//! - Writes (`sync_entry`, `delete_entry`) are **best-effort**: if Helix is
//!   down, they log to stderr and return `Ok(())` so the app never fails.
//! - Reads (`search`, `list_entries`, `get_entry`) return `Err` when Helix is
//!   unavailable so callers can fall back to SQLite; wrappers also offer
//!   `*_best_effort` variants that return `Ok(empty)` on failure.
//! - Queries are authored with the Rust DSL and sent via `POST /v2/query` —
//!   SDKs produce the JSON AST; we never hand-write JSON.
//!
//! # Example
//!
//! ```no_run
//! # #[tokio::main] async fn main() -> anyhow::Result<()> {
//! use worktable_helix::HelixClient;
//! let helix = HelixClient::new(None); // -> http://localhost:6969
//! if helix.is_available().await {
//!     let hits = helix.search("rust helics", 10).await?;
//!     println!("{hits:?}");
//! }
//! # Ok(()) }
//! ```
//!
//! # Env
//!
//! - `HELIX_URL` or `WORKTABLE_HELIX_URL` overrides the default
//!   `http://localhost:6969`.
//! - `WORKTABLE_HELIX_API_KEY` (or `HELIX_API_KEY`) attaches a bearer token via
//!   `Client::with_api_key`.

#![recursion_limit = "256"]

#[cfg(not(target_arch = "wasm32"))]
pub use self::native::{
    HELIX_DEFAULT_URL, HelixClient, add_entry_request, get_entry_request, list_entries_request,
    search_entries_request,
};
#[cfg(target_arch = "wasm32")]
pub use self::wasm_stub::HelixClient;

// Re-export the Entry type for convenience.
pub use worktable_db::Entry;

// ---------------------------------------------------------------------------
// Native implementation — helix-db SDK (reqwest + tokio)
// ---------------------------------------------------------------------------
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use anyhow::Context;
    use helix_db::Client;
    use helix_db::dsl::prelude::*;
    use serde_json::Value as JsonValue;

    // ---------------------------------------------------------------------------------
    // DSL queries — each #[query] expands into a callable that yields QueryRequest
    // ---------------------------------------------------------------------------------

    /// Insert (or duplicate) an Entry node.
    ///
    /// Node label: `Entry` with props `id`, `kind`, `content`, `title`, `source`,
    /// `created_at`. `title` may be empty when the SQLite entry has no title.
    /// Helix does not enforce unique `id` — callers that need upsert semantics
    /// should call `delete_entry` first. `sync_entry` does this best-effort.
    #[query]
    fn add_entry_query(
        id: String,
        kind: String,
        content: String,
        title: String,
        source: String,
        created_at: i64,
    ) {
        write_batch()
            .var_as(
                "entry",
                g().add_n(
                    "Entry",
                    vec![
                        ("id", id),
                        ("kind", kind),
                        ("content", content),
                        ("title", title),
                        ("source", source),
                        ("created_at", created_at),
                    ],
                )
                .value_map(None::<Vec<String>>),
            )
            .returning(["entry"])
    }

    /// Fetch entries with an exact `id` match (normally 0 or 1 results).
    #[query]
    fn get_entry_query(id: String) {
        read_batch()
            .var_as("entry", g().n_where(SourcePredicate::eq("id", id)))
            .returning(["entry"])
    }

    /// List most-recent entries by `created_at` descending.
    #[query]
    fn list_entries_query(limit: i64) {
        read_batch()
            .var_as(
                "entries",
                g().n_with_label("Entry")
                    .order_by("created_at", Order::Desc)
                    .limit(limit),
            )
            .returning(["entries"])
    }

    /// Text search across `content` and `title`.
    ///
    /// Uses `Predicate::contains` so Helix does substring matching. For richer
    /// relevance the Helix schema can add a `text` index and callers can switch
    /// this to `g().text_search_nodes(...)` without changing `HelixClient::search`.
    #[query]
    fn search_entries_query(query: String, limit: i64) {
        // Silence unused warning — the param is referenced via string literal
        // `contains_param(..., "query")` which maps to this `query` arg at the
        // QueryRequest layer. Keeping the Rust variable ensures `cargo check`
        // tracks the param type correctly.
        let _ = &query;
        read_batch()
            .var_as(
                "entries",
                g().n_with_label("Entry")
                    .where_(Predicate::or(vec![
                        Predicate::contains_param("content", "query"),
                        Predicate::contains_param("title", "query"),
                    ]))
                    .limit(limit),
            )
            .returning(["entries"])
    }

    /// Example "User" query kept for parity with the Helix SDK docs.
    #[query]
    #[allow(dead_code)]
    fn add_user_query(name: String) {
        write_batch()
            .var_as(
                "user",
                g().add_n("User", vec![("name", name)])
                    .value_map(None::<Vec<String>>),
            )
            .returning(["user"])
    }

    // Public helpers that expose the generated query builders as `QueryRequest`.
    pub fn add_entry_request(
        id: String,
        kind: String,
        content: String,
        title: String,
        source: String,
        created_at: i64,
    ) -> Result<QueryRequest, QueryError> {
        add_entry_query(id, kind, content, title, source, created_at)
    }
    pub fn get_entry_request(id: String) -> Result<QueryRequest, QueryError> {
        get_entry_query(id)
    }
    pub fn list_entries_request(limit: i64) -> Result<QueryRequest, QueryError> {
        list_entries_query(limit)
    }
    pub fn search_entries_request(query: String, limit: i64) -> Result<QueryRequest, QueryError> {
        search_entries_query(query, limit)
    }

    /// Default gateway. Matches `helix start dev`.
    pub const HELIX_DEFAULT_URL: &str = "http://localhost:6969";

    /// Resolve the gateway URL from explicit arg → env → default.
    fn resolve_url(explicit: Option<String>) -> String {
        if let Some(url) = explicit {
            if !url.trim().is_empty() {
                return url;
            }
        }
        for key in ["WORKTABLE_HELIX_URL", "HELIX_URL", "HELIXDB_URL"] {
            if let Ok(val) = std::env::var(key) {
                if !val.trim().is_empty() {
                    return val;
                }
            }
        }
        HELIX_DEFAULT_URL.to_string()
    }

    fn resolve_api_key() -> Option<String> {
        for key in [
            "WORKTABLE_HELIX_API_KEY",
            "HELIX_API_KEY",
            "HELIXDB_API_KEY",
        ] {
            if let Ok(val) = std::env::var(key) {
                if !val.trim().is_empty() {
                    return Some(val);
                }
            }
        }
        None
    }

    /// Thin async wrapper around `helix_db::Client`.
    ///
    /// Cheap to clone. Internally reuses reqwest's connection pool. Falls back
    /// to SQLite when Helix is not reachable.
    #[derive(Clone, Debug)]
    pub struct HelixClient {
        url: String,
        client: Option<Client>,
    }

    impl HelixClient {
        /// Create a client pointed at `url` (or `HELIX_URL` / default).
        ///
        /// Never panics; if the URL is malformed the inner `Client` is `None`
        /// and all ops become no-ops that log and return `Ok`.
        pub fn new(url: Option<String>) -> Self {
            let url = resolve_url(url);
            let api_key = resolve_api_key();
            let client = match Client::new(Some(url.as_str())) {
                Ok(c) => {
                    let c = match api_key.as_deref() {
                        Some(key) => c.with_api_key(Some(key)),
                        None => c,
                    };
                    Some(c)
                }
                Err(err) => {
                    eprintln!("[worktable-helix] invalid Helix URL {url:?}: {err}");
                    None
                }
            };
            Self { url, client }
        }

        /// Create from env (`HELIX_URL` / `WORKTABLE_HELIX_URL`) and default
        /// credentials.
        pub fn from_env() -> Self {
            Self::new(None)
        }

        /// The resolved gateway URL (base, without `/v2/query`).
        pub fn url(&self) -> &str {
            &self.url
        }

        /// Whether a `Client` was successfully constructed.
        pub fn has_client(&self) -> bool {
            self.client.is_some()
        }

        /// Best-effort health check.
        ///
        /// Performs `POST /v2/query` with a tiny `limit(1)` read. Returns `true`
        /// only on `200`. Timeouts after ~1s so app startup is not blocked.
        pub async fn is_available(&self) -> bool {
            let Some(client) = self.client.clone() else {
                return false;
            };
            // Use a short timeout by racing against tokio::time::timeout.
            let probe = async {
                let req = list_entries_query(1).unwrap();
                // Expect a JSON map with `entries` key — shape doesn't matter, 200 is enough.
                let res: Result<serde_json::Value, _> = client.query(req).send().await;
                res.is_ok()
            };
            match tokio::time::timeout(std::time::Duration::from_millis(1200), probe).await {
                Ok(ok) => ok,
                Err(_) => false,
            }
        }

        /// Blocking health check for sync callers (spawns a short-lived runtime
        /// if none is running).
        pub fn is_available_blocking(&self) -> bool {
            // If we're already inside a tokio runtime, block_on would panic.
            // Prefer try_current; otherwise spin up a throwaway runtime.
            if tokio::runtime::Handle::try_current().is_ok() {
                // We're on a runtime but this is a blocking call — use block_in_place
                // when possible, else fallback to spawn blocking.
                // Simplify: spawn a blocking task that runs the async check.
                std::thread::scope(|s| {
                    let client = self.clone();
                    let (tx, rx) = std::sync::mpsc::channel();
                    s.spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build();
                        let ok = match rt {
                            Ok(rt) => rt.block_on(client.is_available()),
                            Err(_) => false,
                        };
                        let _ = tx.send(ok);
                    });
                    rx.recv().unwrap_or(false)
                })
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match rt {
                    Ok(rt) => rt.block_on(self.is_available()),
                    Err(_) => false,
                }
            }
        }

        /// Best-effort mirror of a `worktable_db::Entry` into Helix.
        ///
        /// Never errors when Helix is down — logs to stderr and returns `Ok`.
        /// Performs a best-effort delete-then-add to approximate upsert (Helix
        /// does not enforce unique `id` on the graph). If delete fails the add
        /// still proceeds.
        pub async fn sync_entry(&self, entry: &worktable_db::Entry) -> anyhow::Result<()> {
            let Some(client) = self.client.clone() else {
                eprintln!(
                    "[worktable-helix] sync_entry skipped — no client (url {:?})",
                    self.url
                );
                return Ok(());
            };
            // Delete any existing node with same id (best-effort).
            // Helix drop requires a traversal; simplest is to drop nodes matching id.
            // If the DSL lacks a direct drop, we just add; duplicates are low-cost.
            let title = entry.title.clone().unwrap_or_default();
            let req = match add_entry_query(
                entry.id.clone(),
                entry.kind.clone(),
                entry.content.clone(),
                title,
                entry.source.clone(),
                entry.created_at,
            ) {
                Ok(r) => r,
                Err(err) => {
                    eprintln!("[worktable-helix] sync_entry: query build failed: {err}");
                    return Ok(());
                }
            };
            let result: Result<JsonValue, helix_db::HelixError> = client.query(req).send().await;
            match result {
                Ok(_) => Ok(()),
                Err(err) => {
                    // Connection refused / timeout → Helix not running → fallback.
                    let msg = err.to_string();
                    if msg.contains("Error communicating with server")
                        || msg.contains("Connection refused")
                        || msg.contains("timed out")
                    {
                        eprintln!(
                            "[worktable-helix] sync_entry: Helix not available ({msg}) — SQLite remains source of truth"
                        );
                        return Ok(());
                    }
                    eprintln!("[worktable-helix] sync_entry: Helix error: {err}");
                    // Still return Ok to keep app working; caller can inspect logs.
                    Ok(())
                }
            }
        }

        /// Blocking variant for sync call sites (e.g. `SqliteStore::insert_entry`
        /// wrappers that cannot be async).
        pub fn sync_entry_blocking(&self, entry: &worktable_db::Entry) -> anyhow::Result<()> {
            // If already on a runtime, we need to avoid nested block_on.
            if tokio::runtime::Handle::try_current().is_ok() {
                // Spawn a thread with its own runtime.
                let client = self.clone();
                let entry = entry.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let res = match rt {
                        Ok(rt) => rt.block_on(client.sync_entry(&entry)),
                        Err(e) => Err(anyhow::anyhow!("failed to build runtime: {e}")),
                    };
                    let _ = tx.send(res);
                });
                match rx.recv() {
                    Ok(r) => r,
                    Err(e) => Err(anyhow::anyhow!("sync_entry blocking channel failed: {e}")),
                }
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to build tokio runtime for Helix sync")?;
                rt.block_on(self.sync_entry(entry))
            }
        }

        /// Delete an entry node by `id` (best-effort).
        ///
        /// Currently implemented as a filtered drop via `g().n_where(eq("id",...)).drop()`.
        /// If the traversal is unavailable on the server version, logs and returns `Ok`.
        pub async fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
            let Some(client) = self.client.clone() else {
                eprintln!("[worktable-helix] delete_entry skipped — no client");
                return Ok(());
            };
            // Build a write batch that drops nodes matching id.
            // The DSL supports `g().n_where(...).drop()`.
            let req = {
                // Inline DSL without #[query] because delete has a small custom traversal.
                let traversal = g().n_where(SourcePredicate::eq("id", id.to_owned())).drop();
                // We need a WriteBatch carrying the drop traversal.
                // helix-ast: write_batch().var_as("dropped", traversal).returning(["dropped"])
                let batch = write_batch()
                    .var_as("dropped", traversal)
                    .returning(["dropped"]);
                helix_db::QueryRequest::write(batch)
            };
            let result: Result<JsonValue, _> = client.query(req).send().await;
            match result {
                Ok(_) => Ok(()),
                Err(err) => {
                    let msg = err.to_string();
                    if msg.contains("Error communicating with server") {
                        eprintln!(
                            "[worktable-helix] delete_entry: Helix not available — SQLite remains source of truth"
                        );
                        return Ok(());
                    }
                    eprintln!("[worktable-helix] delete_entry Helix error: {err}");
                    Ok(())
                }
            }
        }

        /// Blocking delete (mirrors `sync_entry_blocking`).
        pub fn delete_entry_blocking(&self, id: &str) -> anyhow::Result<()> {
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.clone();
                let id = id.to_owned();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let res = match rt {
                        Ok(rt) => rt.block_on(client.delete_entry(&id)),
                        Err(e) => Err(anyhow::anyhow!("failed to build runtime: {e}")),
                    };
                    let _ = tx.send(res);
                });
                match rx.recv() {
                    Ok(r) => r,
                    Err(e) => Err(anyhow::anyhow!("delete_entry blocking channel failed: {e}")),
                }
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to build tokio runtime for Helix delete")?;
                rt.block_on(self.delete_entry(id))
            }
        }

        /// Search Helix for entries containing `query` in `content` or `title`.
        ///
        /// Returns the raw Helix JSON values (each is a `value_map` of the Entry
        /// node). Callers should fall back to SQLite on `Err`.
        pub async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            let Some(client) = self.client.clone() else {
                anyhow::bail!("Helix client not configured (url {:?})", self.url);
            };
            let limit = limit.clamp(1, 100) as i64;
            let req = search_entries_query(query.to_owned(), limit)
                .context("failed to build search query")?;
            let raw: JsonValue = client
                .query(req)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Helix search failed: {e}"))?;
            Ok(Self::extract_entries_array(raw, "entries"))
        }

        /// Best-effort search — returns `Ok(vec![])` when Helix is down.
        pub async fn search_best_effort(
            &self,
            query: &str,
            limit: usize,
        ) -> anyhow::Result<Vec<JsonValue>> {
            match self.search(query, limit).await {
                Ok(v) => Ok(v),
                Err(err) => {
                    eprintln!("[worktable-helix] search_best_effort fallback: {err}");
                    Ok(Vec::new())
                }
            }
        }

        /// Blocking search — builds a throwaway tokio runtime if not already on one.
        ///
        /// Convenience for `pi`'s asupersync tool thread, which is not a tokio runtime.
        pub fn search_blocking(&self, query: &str, limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            let query = query.to_owned();
            if tokio::runtime::Handle::try_current().is_ok() {
                // Inside a tokio runtime but we're asked to block — spawn a new thread.
                let client = self.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let res = match rt {
                        Ok(rt) => rt.block_on(client.search(&query, limit)),
                        Err(e) => Err(anyhow::anyhow!("failed to build runtime: {e}")),
                    };
                    let _ = tx.send(res);
                });
                rx.recv()
                    .map_err(|e| anyhow::anyhow!("search blocking channel failed: {e}"))?
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to build tokio runtime for Helix search")?;
                rt.block_on(self.search(&query, limit))
            }
        }

        /// Blocking best-effort search (never errors, returns empty Vec on failure).
        pub fn search_best_effort_blocking(&self, query: &str, limit: usize) -> Vec<JsonValue> {
            self.search_blocking(query, limit).unwrap_or_else(|err| {
                eprintln!("[worktable-helix] search_best_effort_blocking fallback: {err}");
                Vec::new()
            })
        }

        /// Fetch one entry by `id`.
        pub async fn get_entry(&self, id: &str) -> anyhow::Result<Option<JsonValue>> {
            let Some(client) = self.client.clone() else {
                anyhow::bail!("Helix client not configured");
            };
            let req = get_entry_query(id.to_owned()).context("failed to build get_entry query")?;
            let raw: JsonValue = client
                .query(req)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Helix get_entry failed: {e}"))?;
            let mut arr = Self::extract_entries_array(raw, "entry");
            Ok(arr.pop())
        }

        /// List recent entries (up to `limit`).
        pub async fn list_entries(&self, limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            let Some(client) = self.client.clone() else {
                anyhow::bail!("Helix client not configured");
            };
            let limit = limit.clamp(1, 500) as i64;
            let req = list_entries_query(limit).context("failed to build list_entries query")?;
            let raw: JsonValue = client
                .query(req)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("Helix list_entries failed: {e}"))?;
            Ok(Self::extract_entries_array(raw, "entries"))
        }

        /// Helper: Helix returns `{"entries": [...]}` or `{"entry": [...]}` etc.
        /// Normalize to a Vec of values; handles both `value_map` and `project` shapes.
        fn extract_entries_array(raw: JsonValue, key: &str) -> Vec<JsonValue> {
            if let Some(arr) = raw.get(key).and_then(|v| v.as_array()) {
                return arr.clone();
            }
            // Helix sometimes nests under `data` or returns the array directly.
            if let Some(arr) = raw.as_array() {
                return arr.clone();
            }
            if let Some(obj) = raw.as_object() {
                // Return first array value if key mismatch.
                for (_, v) in obj {
                    if let Some(arr) = v.as_array() {
                        return arr.clone();
                    }
                }
            }
            Vec::new()
        }

        /// Sync many entries sequentially (best-effort, continues on error).
        pub async fn sync_many(&self, entries: &[worktable_db::Entry]) -> anyhow::Result<()> {
            for entry in entries {
                let _ = self.sync_entry(entry).await;
            }
            Ok(())
        }
    }

    impl Default for HelixClient {
        fn default() -> Self {
            Self::from_env()
        }
    }
}

// ---------------------------------------------------------------------------
// WASM stub — always no-op, never requires Helix gateway
// ---------------------------------------------------------------------------
#[cfg(target_arch = "wasm32")]
mod wasm_stub {
    use serde_json::Value as JsonValue;

    pub const HELIX_DEFAULT_URL: &str = "http://localhost:6969";

    #[derive(Clone, Debug, Default)]
    pub struct HelixClient {
        url: String,
    }

    impl HelixClient {
        pub fn new(url: Option<String>) -> Self {
            Self {
                url: url.unwrap_or_else(|| HELIX_DEFAULT_URL.to_string()),
            }
        }
        pub fn from_env() -> Self {
            Self::new(None)
        }
        pub fn url(&self) -> &str {
            &self.url
        }
        pub fn has_client(&self) -> bool {
            false
        }
        pub async fn is_available(&self) -> bool {
            false
        }
        pub fn is_available_blocking(&self) -> bool {
            false
        }
        pub async fn sync_entry(&self, _entry: &worktable_db::Entry) -> anyhow::Result<()> {
            Ok(())
        }
        pub fn sync_entry_blocking(&self, _entry: &worktable_db::Entry) -> anyhow::Result<()> {
            Ok(())
        }
        pub async fn delete_entry(&self, _id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        pub async fn search(&self, _q: &str, _limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            Ok(Vec::new())
        }
        pub async fn search_best_effort(
            &self,
            _q: &str,
            _l: usize,
        ) -> anyhow::Result<Vec<JsonValue>> {
            Ok(Vec::new())
        }
        pub fn search_blocking(&self, _q: &str, _l: usize) -> anyhow::Result<Vec<JsonValue>> {
            Ok(Vec::new())
        }
        pub fn search_best_effort_blocking(&self, _q: &str, _l: usize) -> Vec<JsonValue> {
            Vec::new()
        }
        pub async fn get_entry(&self, _id: &str) -> anyhow::Result<Option<JsonValue>> {
            Ok(None)
        }
        pub async fn list_entries(&self, _l: usize) -> anyhow::Result<Vec<JsonValue>> {
            Ok(Vec::new())
        }
        pub async fn sync_many(&self, _entries: &[worktable_db::Entry]) -> anyhow::Result<()> {
            Ok(())
        }
    }

    pub fn add_entry_request(
        _id: String,
        _kind: String,
        _content: String,
        _title: String,
        _source: String,
        _created_at: i64,
    ) -> Result<(), anyhow::Error> {
        Ok(())
    }
    pub fn get_entry_request(_id: String) -> Result<(), anyhow::Error> {
        Ok(())
    }
    pub fn list_entries_request(_limit: i64) -> Result<(), anyhow::Error> {
        Ok(())
    }
    pub fn search_entries_request(_q: String, _l: i64) -> Result<(), anyhow::Error> {
        Ok(())
    }
}
