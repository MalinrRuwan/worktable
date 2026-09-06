//! Worktable Helix — embedded knowledge graph (topics + relations) mirroring SQLite.
//!
//! SQLite (`~/.worktable/worktable.db`) is source-of-truth. This crate maintains
//! a parallel graph at `~/.worktable/helix.json` (same dir as the DB) with:
//! - `Entry` nodes (id, kind, content, title, source, created_at, topics: Vec<String>)
//! - `Topic` nodes (name) + `Entry -HAS_TOPIC-> Topic` edges
//! - `Entry -RELATED-> Entry` edges weighted by shared topics (Jaccard)
//! so the AI agent can find relevant entries via `search_knowledge` without
//! needing a separate Helix process. All writes are best-effort and never fail
//! the SQLite insert.
//!
//! The DSL is still `helix-db` style: `read_batch`/`write_batch` + `g()` traversals,
//! but for the embedded file we execute them directly against the JSON graph.
//!
//! A button next to Send in the AI pane triggers `build_from_sqlite` — if the
//! graph is already built with the latest entry it is kept, otherwise it syncs
//! new entries. New `insert_entry` calls also auto-sync.

#![recursion_limit = "256"]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

pub use worktable_db::Entry;

#[cfg(not(target_arch = "wasm32"))]
pub use self::native::{
    HelixClient, add_entry_request, get_entry_request, list_entries_request,
    search_entries_request, HELIX_DEFAULT_URL,
};
#[cfg(target_arch = "wasm32")]
pub use self::wasm_stub::HelixClient;

// ---------------------------------------------------------------------------
// Helpers: path next to SQLite
// ---------------------------------------------------------------------------

pub fn helix_path_for_sqlite(sqlite_path: &str) -> PathBuf {
    let p = Path::new(sqlite_path);
    let dir = p.parent().unwrap_or_else(|| Path::new("."));
    dir.join("helix.json")
}

pub fn default_helix_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    Path::new(&home).join(".worktable").join("helix.json")
}

/// Common English + markdown/URL stopwords that make useless topics.
const TOPIC_STOPWORDS: &[&str] = &[
    // Worktable domain
    "this", "that", "with", "from", "have", "will", "your", "about", "hello", "world",
    "item", "link", "text", "worktable", "entry", "note", "image", "photo", "file",
    // Common English
    "the", "and", "for", "are", "but", "not", "you", "all", "can", "had", "her",
    "was", "one", "our", "out", "day", "get", "has", "him", "his", "how", "its",
    "may", "new", "now", "old", "see", "two", "way", "who", "did", "let", "put",
    "say", "she", "too", "use", "they", "them", "then", "than", "when", "what",
    "where", "which", "while", "there", "their", "been", "being", "some", "such",
    "only", "over", "also", "into", "just", "like", "make", "made", "more", "most",
    "much", "many", "very", "each", "even", "here", "these", "those", "through",
    "should", "would", "could", "shall", "must", "need", "needs", "someone",
    "something", "anything", "everything", "because", "before", "after", "between",
    "without", "within", "against", "under", "above", "below", "same", "other",
    "another", "every", "both", "few", "own", "once", "using", "used", "uses",
    // Markdown / URL / path noise
    "http", "https", "www", "com", "org", "net", "html", "htm", "php", "aspx",
    "png", "jpg", "jpeg", "gif", "webp", "svg", "tmp", "var", "users", "home",
    "desktop", "documents", "downloads", "untitled", "screenshot", "screen", "shot",
];

pub fn topic_for_entry(entry: &Entry) -> Vec<String> {
    // Small keyword extractor: split into words, drop stopwords, rank by
    // frequency (title words count triple — the title is the summary), and
    // break ties by first occurrence so the *leading* subject of the note
    // wins over an arbitrary alphabetical word.
    let title = entry.title.clone().unwrap_or_default();
    let lower_title = title.to_lowercase();
    let lower_content = entry.content.to_lowercase();

    let stop: std::collections::HashSet<&str> = TOPIC_STOPWORDS.iter().copied().collect();
    let tokenize = |text: &str| -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() > 3)
            // Pure numbers and version-ish tokens ("2024", "3d1f") are noise.
            .filter(|w| !w.chars().all(|c| c.is_ascii_digit()))
            .filter(|w| !stop.contains(w))
            .map(|w| w.to_string())
            .collect()
    };

    let title_words = tokenize(&lower_title);
    let content_words = tokenize(&lower_content);

    // freq + first-occurrence index across title-then-content order.
    let mut freq: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut first_seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut position = 0usize;
    for word in title_words.iter().chain(content_words.iter()) {
        let weight = if position < title_words.len() { 3 } else { 1 };
        *freq.entry(word.clone()).or_insert(0) += weight;
        first_seen.entry(word.clone()).or_insert(position);
        position += 1;
    }

    let mut sorted: Vec<(String, usize)> = freq.into_iter().collect();
    sorted.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| {
            let ia = first_seen.get(&a.0).copied().unwrap_or(usize::MAX);
            let ib = first_seen.get(&b.0).copied().unwrap_or(usize::MAX);
            ia.cmp(&ib).then_with(|| a.0.cmp(&b.0))
        })
    });
    sorted.into_iter().take(5).map(|(k, _)| k).collect()
}

// ---------------------------------------------------------------------------
// Native: embedded JSON graph + still support HTTP fallback
// ---------------------------------------------------------------------------
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use helix_db::dsl::prelude::*;
    use serde_json::Value as JsonValue;

    // Keep the DSL queries for parity with the HTTP SDK — they are still
    // generated but for the embedded file we execute a direct in-memory search.
    #[query]
    fn add_entry_query(id: String, kind: String, content: String, title: String, source: String, created_at: i64) -> WriteBatch {
        write_batch().var_as("entry", g().add_n("Entry", vec![("id", id), ("kind", kind), ("content", content), ("title", title), ("source", source), ("created_at", created_at)]).value_map(None::<Vec<String>>)).returning(["entry"])
    }
    #[query] #[allow(unused_braces)] fn get_entry_query(id: String) -> ReadBatch { read_batch().var_as("entry", g().n_where(SourcePredicate::eq("id", id))).returning(["entry"]) }
    #[query] #[allow(unused_braces)] fn list_entries_query(limit: i64) -> ReadBatch { read_batch().var_as("entries", g().n_with_label("Entry").order_by("created_at", Order::Desc).limit(limit)).returning(["entries"]) }
    #[query] fn search_entries_query(query: String, limit: i64) -> ReadBatch {
        let _ = &query;
        read_batch().var_as("entries", g().n_with_label("Entry").where_(Predicate::or(vec![Predicate::contains_param("content", "query"), Predicate::contains_param("title", "query")])).limit(limit)).returning(["entries"])
    }
    pub fn add_entry_request(id: String, kind: String, content: String, title: String, source: String, created_at: i64) -> Result<QueryRequest, QueryError> { add_entry_query(id, kind, content, title, source, created_at) }
    pub fn get_entry_request(id: String) -> Result<QueryRequest, QueryError> { get_entry_query(id) }
    pub fn list_entries_request(limit: i64) -> Result<QueryRequest, QueryError> { list_entries_query(limit) }
    pub fn search_entries_request(query: String, limit: i64) -> Result<QueryRequest, QueryError> { search_entries_query(query, limit) }

    pub const HELIX_DEFAULT_URL: &str = "http://localhost:6969";

    #[derive(Clone, Debug, Serialize, Deserialize, Default)]
    struct Graph {
        entries: std::collections::HashMap<String, worktable_db::Entry>,
        entry_topics: std::collections::HashMap<String, Vec<String>>, // id -> topics
        topic_entries: std::collections::HashMap<String, Vec<String>>, // topic -> ids
        // For the embedded file we also store a simple relations cache
        relations: std::collections::HashMap<(String, String), f32>, // (id1,id2) -> jaccard
    }

    impl Graph {
        fn topics_for(&self, id: &str) -> Vec<String> {
            self.entry_topics.get(id).cloned().unwrap_or_default()
        }
        fn rebuild_relations(&mut self) {
            self.relations.clear();
            let ids: Vec<String> = self.entries.keys().cloned().collect();
            for i in 0..ids.len() {
                for j in (i+1)..ids.len() {
                    let a = &ids[i];
                    let b = &ids[j];
                    let ta: std::collections::HashSet<String> = self.topics_for(a).into_iter().collect();
                    let tb: std::collections::HashSet<String> = self.topics_for(b).into_iter().collect();
                    if ta.is_empty() || tb.is_empty() { continue; }
                    let inter = ta.intersection(&tb).count() as f32;
                    let uni = ta.union(&tb).count() as f32;
                    let jaccard = inter / uni;
                    if jaccard > 0.1 {
                        self.relations.insert((a.clone(), b.clone()), jaccard);
                        self.relations.insert((b.clone(), a.clone()), jaccard);
                    }
                }
            }
        }
    }

    #[derive(Clone, Debug)]
    pub struct HelixClient {
        path: PathBuf,
        graph: Arc<Mutex<Graph>>,
        http_url: String,
    }

    impl HelixClient {
        pub fn new(url: Option<String>) -> Self {
            let url = url.unwrap_or_else(|| super::default_helix_path().to_string_lossy().to_string());
            // Try to treat url as path if it's a file path, otherwise as http url
            let path = if url.starts_with("http") {
                super::default_helix_path()
            } else {
                PathBuf::from(&url)
            };
            Self::open_embedded(path)
        }
        pub fn from_env() -> Self {
            // Prefer embedded file next to DB, fallback to HTTP for compat
            let sqlite_path = std::env::var("WORKTABLE_DB_PATH").ok()
                .or_else(|| std::env::var("HOME").ok().map(|h| format!("{}/.worktable/worktable.db", h)))
                .unwrap_or_else(|| "/tmp/worktable.db".to_string());
            let path = super::helix_path_for_sqlite(&sqlite_path);
            Self::open_embedded(path)
        }
        pub fn open_embedded(path: PathBuf) -> Self {
            let graph = Arc::new(Mutex::new(Graph::default()));
            let client = Self {
                path: path.clone(),
                graph: graph.clone(),
                http_url: HELIX_DEFAULT_URL.to_string(),
            };
            // Load existing file if present
            if path.exists() {
                if let Ok(data) = std::fs::read_to_string(&path) {
                    if let Ok(g) = serde_json::from_str::<Graph>(&data) {
                        *client.graph.lock().unwrap() = g;
                    }
                }
            }
            client
        }
        pub fn url(&self) -> &str { &self.http_url }
        pub fn has_client(&self) -> bool { true } // embedded always has client
        fn save(&self) {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(data) = serde_json::to_string_pretty(&*self.graph.lock().unwrap()) {
                let _ = std::fs::write(&self.path, data);
            }
        }
        pub async fn is_available(&self) -> bool { self.path.exists() || true }
        pub fn is_available_blocking(&self) -> bool { true }
        pub async fn sync_entry(&self, entry: &worktable_db::Entry) -> anyhow::Result<()> {
            let topics = super::topic_for_entry(entry);
            let mut g = self.graph.lock().unwrap();
            g.entries.insert(entry.id.clone(), entry.clone());
            g.entry_topics.insert(entry.id.clone(), topics.clone());
            for t in &topics {
                g.topic_entries.entry(t.clone()).or_default().push(entry.id.clone());
                // dedup
                let v = g.topic_entries.get_mut(t).unwrap();
                v.sort(); v.dedup();
            }
            g.rebuild_relations();
            drop(g);
            self.save();
            Ok(())
        }
        pub fn sync_entry_blocking(&self, entry: &worktable_db::Entry) -> anyhow::Result<()> {
            // Use the async version via a throwaway runtime if needed
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.clone();
                let entry = entry.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let res = rt.block_on(client.sync_entry(&entry));
                    let _ = tx.send(res);
                });
                rx.recv().unwrap()
            } else {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(self.sync_entry(entry))
            }
        }
        pub async fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
            let mut g = self.graph.lock().unwrap();
            g.entries.remove(id);
            if let Some(topics) = g.entry_topics.remove(id) {
                for t in topics {
                    if let Some(v) = g.topic_entries.get_mut(&t) {
                        v.retain(|x| x != id);
                        if v.is_empty() { g.topic_entries.remove(&t); }
                    }
                }
            }
            g.rebuild_relations();
            drop(g);
            self.save();
            Ok(())
        }
        pub fn delete_entry_blocking(&self, id: &str) -> anyhow::Result<()> {
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.clone(); let id = id.to_owned();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let res = rt.block_on(client.delete_entry(&id));
                    let _ = tx.send(res);
                });
                rx.recv().unwrap()
            } else {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(self.delete_entry(id))
            }
        }
        pub async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            let q = query.to_lowercase();
            let g = self.graph.lock().unwrap();
            let mut scored: Vec<(f32, &worktable_db::Entry)> = Vec::new();
            for entry in g.entries.values() {
                let hay = format!("{} {} {}", entry.title.clone().unwrap_or_default(), entry.content, entry.source).to_lowercase();
                let mut score = 0.0;
                // Direct substring hit against title/content/source.
                if hay.contains(&q) { score += 10.0; }
                // Topic boost: only when the *query* relates to one of the
                // entry's topics. (Boosting for `hay.contains(topic)` would
                // score every entry on every query, since an entry's own
                // topics always appear in its text.)
                for t in g.topics_for(&entry.id) {
                    if q.contains(&t) || t.contains(&q) { score += 5.0; }
                }
                if score > 0.0 {
                    scored.push((score, entry));
                }
            }
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            let out: Vec<JsonValue> = scored.into_iter().take(limit).map(|(_, e)| serde_json::to_value(e).unwrap()).collect();
            Ok(out)
        }
        pub async fn search_best_effort(&self, query: &str, limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            Ok(self.search(query, limit).await.unwrap_or_default())
        }
        pub fn search_blocking(&self, query: &str, limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.clone(); let q = query.to_owned();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let res = rt.block_on(client.search(&q, limit));
                    let _ = tx.send(res);
                });
                rx.recv().unwrap()
            } else {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(self.search(query, limit))
            }
        }
        pub fn search_best_effort_blocking(&self, query: &str, limit: usize) -> Vec<JsonValue> {
            self.search_blocking(query, limit).unwrap_or_default()
        }
        /// Build or sync the graph from SQLite — if the graph already has the latest entry, keep it; otherwise sync new ones.
        pub async fn build_from_sqlite(&self, sqlite_path: &str) -> anyhow::Result<usize> {
            let store = worktable_db::SqliteStore::connect(sqlite_path)?;
            store.migrate()?;
            let entries = store.list_entries(5000)?;
            let mut synced = 0;
            let existing: std::collections::HashSet<String> = {
                self.graph.lock().unwrap().entries.keys().cloned().collect()
            };
            for entry in entries {
                if !existing.contains(&entry.id) {
                    self.sync_entry(&entry).await?;
                    synced += 1;
                }
            }
            // If no new entries but graph was empty and we have entries, the loop above handled it.
            // Also ensure topics are rebuilt (sync_entry already does).
            Ok(synced)
        }
        pub fn build_from_sqlite_blocking(&self, sqlite_path: &str) -> anyhow::Result<usize> {
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.clone(); let path = sqlite_path.to_owned();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    let res = rt.block_on(client.build_from_sqlite(&path));
                    let _ = tx.send(res);
                });
                rx.recv().unwrap()
            } else {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(self.build_from_sqlite(sqlite_path))
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_stub {
    use super::Entry;
    #[derive(Clone, Debug, Default)]
    pub struct HelixClient;
    impl HelixClient {
        pub fn new(_url: Option<String>) -> Self { Self }
        pub fn from_env() -> Self { Self }
        pub fn url(&self) -> &str { "wasm-stub" }
        pub fn has_client(&self) -> bool { false }
        pub async fn is_available(&self) -> bool { false }
        pub fn is_available_blocking(&self) -> bool { false }
        pub async fn sync_entry(&self, _entry: &Entry) -> anyhow::Result<()> { Ok(()) }
        pub fn sync_entry_blocking(&self, _entry: &Entry) -> anyhow::Result<()> { Ok(()) }
        pub async fn delete_entry(&self, _id: &str) -> anyhow::Result<()> { Ok(()) }
        pub fn delete_entry_blocking(&self, _id: &str) -> anyhow::Result<()> { Ok(()) }
        pub async fn search(&self, _query: &str, _limit: usize) -> anyhow::Result<Vec<serde_json::Value>> { Ok(vec![]) }
        pub async fn search_best_effort(&self, _q: &str, _l: usize) -> anyhow::Result<Vec<serde_json::Value>> { Ok(vec![]) }
        pub fn search_blocking(&self, _q: &str, _l: usize) -> anyhow::Result<Vec<serde_json::Value>> { Ok(vec![]) }
        pub fn search_best_effort_blocking(&self, _q: &str, _l: usize) -> Vec<serde_json::Value> { vec![] }
        pub async fn build_from_sqlite(&self, _path: &str) -> anyhow::Result<usize> { Ok(0) }
        pub fn build_from_sqlite_blocking(&self, _path: &str) -> anyhow::Result<usize> { Ok(0) }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(title: Option<&str>, content: &str) -> Entry {
        Entry {
            id: "t".to_owned(),
            kind: "text".to_owned(),
            content: content.to_owned(),
            title: title.map(|t| t.to_owned()),
            source: "Worktable".to_owned(),
            created_at: 0,
        }
    }

    #[test]
    fn topic_extractor_leading_subject_wins_on_ties() {
        // All words appear once — the *first* meaningful word is the subject.
        let topics = topic_for_entry(&entry(
            None,
            "Use TOML as the default declarative format, backed by a published schema",
        ));
        assert_eq!(topics.first().map(String::as_str), Some("toml"));

        let topics = topic_for_entry(&entry(
            None,
            "gitignore — universal ignore rules for versioned files",
        ));
        assert_eq!(topics.first().map(String::as_str), Some("gitignore"));

        let topics = topic_for_entry(&entry(
            None,
            "Negation in inherited configs. The moment a config can extend a base or preset, someone needs to remove an extension",
        ));
        assert_eq!(topics.first().map(String::as_str), Some("negation"));
    }

    #[test]
    fn topic_extractor_prefers_frequent_words() {
        let topics = topic_for_entry(&entry(None, "alpha alpha alpha beta beta gamma"));
        assert_eq!(topics.first().map(String::as_str), Some("alpha"));
        assert!(topics.contains(&"beta".to_owned()));
    }

    #[test]
    fn topic_extractor_weights_title_over_content() {
        // "schema" appears once in the title; "parser" twice in the body.
        // Title weight (×3) should win.
        let topics = topic_for_entry(&entry(
            Some("Schema design"),
            "parser internals and the parser pipeline",
        ));
        assert_eq!(topics.first().map(String::as_str), Some("schema"));
    }

    #[test]
    fn topic_extractor_drops_stopwords_numbers_and_paths() {
        let topics = topic_for_entry(&entry(
            None,
            "this that with from 2024 2025 /tmp/photo.png https://example.com/page.html",
        ));
        for junk in [
            "this", "that", "with", "from", "2024", "2025", "tmp", "photo", "png", "https",
            "com", "html",
        ] {
            assert!(
                !topics.contains(&junk.to_owned()),
                "{junk} should never be a topic"
            );
        }
    }

    #[test]
    fn topic_extractor_returns_empty_for_pure_stopword_text() {
        let topics = topic_for_entry(&entry(None, "this that with from"));
        assert!(topics.is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn graph_sync_search_and_delete_roundtrip() {
        let dir = std::env::temp_dir().join(format!("helix-test-{}", uuid_v4()));
        let path = dir.join("helix.json");
        let client = HelixClient::open_embedded(path.clone());

        let mut e1 = entry(None, "The sunset over the mountains was breathtaking");
        e1.id = "e1".to_owned();
        let mut e2 = entry(Some("Vision Transformer"), "/tmp/vision_transformer.png");
        e2.id = "e2".to_owned();
        e2.kind = "image".to_owned();

        client.sync_entry_blocking(&e1).unwrap();
        client.sync_entry_blocking(&e2).unwrap();
        assert!(path.exists(), "graph should persist to disk");

        let hits = client.search_blocking("sunset", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["id"], "e1");

        // Title is searchable too.
        let hits = client.search_blocking("vision", 10).unwrap();
        assert!(hits.iter().any(|h| h["id"] == "e2"));

        // Reload from disk in a fresh client — persistence check.
        let reloaded = HelixClient::open_embedded(path.clone());
        let hits = reloaded.search_blocking("sunset", 10).unwrap();
        assert_eq!(hits.len(), 1);

        client.delete_entry_blocking("e1").unwrap();
        let hits = client.search_blocking("sunset", 10).unwrap();
        assert!(hits.is_empty(), "deleted entry must not be searchable");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn graph_search_ranks_substring_matches_first() {
        let dir = std::env::temp_dir().join(format!("helix-test-{}", uuid_v4()));
        let client = HelixClient::open_embedded(dir.join("helix.json"));

        let mut e1 = entry(None, "alpha beta gamma");
        e1.id = "direct".to_owned();
        let mut e2 = entry(None, "completely unrelated words delta");
        e2.id = "indirect".to_owned();
        client.sync_entry_blocking(&e1).unwrap();
        client.sync_entry_blocking(&e2).unwrap();

        let hits = client.search_blocking("alpha", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["id"], "direct");

        // Empty-ish query matches nothing (no substring hit, no topic hit).
        let hits = client.search_blocking("zzz-not-present", 10).unwrap();
        assert!(hits.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tiny UUID stand-in so the helix crate doesn't need the uuid dependency.
    #[cfg(not(target_arch = "wasm32"))]
    fn uuid_v4() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{}", std::process::id(), nanos)
    }
}
