//! Worktable Helix — embedded knowledge graph (topics + relations) mirroring SQLite.
//!
//! SQLite (`~/.worktable/worktable.db`) is source-of-truth. This crate maintains
//! a parallel graph at `~/.worktable/helix.json` (same dir as the DB) with:
//! - `Entry` nodes (id, content, title, source, created_at, topics: Vec<String>)
//! - `Topic` nodes (name) + `Entry -HAS_TOPIC-> Topic` edges
//! - `Entry -RELATED-> Entry` edges weighted by shared topics (Jaccard)
//!
//! So the AI agent can find relevant entries via `search_knowledge` without
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

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub use worktable_db::Entry;

#[cfg(not(target_arch = "wasm32"))]
pub use self::native::{
    HELIX_DEFAULT_URL, HelixClient, add_entry_request, get_entry_request, list_entries_request,
    search_entries_request,
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
    "this",
    "that",
    "with",
    "from",
    "have",
    "will",
    "your",
    "about",
    "hello",
    "world",
    "item",
    "link",
    "text",
    "worktable",
    "entry",
    "note",
    "image",
    "photo",
    "file",
    // Common English
    "the",
    "and",
    "for",
    "are",
    "but",
    "not",
    "you",
    "all",
    "can",
    "had",
    "her",
    "was",
    "one",
    "our",
    "out",
    "day",
    "get",
    "has",
    "him",
    "his",
    "how",
    "its",
    "may",
    "new",
    "now",
    "old",
    "see",
    "two",
    "way",
    "who",
    "did",
    "let",
    "put",
    "say",
    "she",
    "too",
    "use",
    "they",
    "them",
    "then",
    "than",
    "when",
    "what",
    "where",
    "which",
    "while",
    "there",
    "their",
    "been",
    "being",
    "some",
    "such",
    "only",
    "over",
    "also",
    "into",
    "just",
    "like",
    "make",
    "made",
    "more",
    "most",
    "much",
    "many",
    "very",
    "each",
    "even",
    "here",
    "these",
    "those",
    "through",
    "should",
    "would",
    "could",
    "shall",
    "must",
    "need",
    "needs",
    "someone",
    "something",
    "anything",
    "everything",
    "because",
    "before",
    "after",
    "between",
    "without",
    "within",
    "against",
    "under",
    "above",
    "below",
    "same",
    "other",
    "another",
    "every",
    "both",
    "few",
    "own",
    "once",
    "using",
    "used",
    "uses",
    // Markdown / URL / path noise
    "http",
    "https",
    "www",
    "com",
    "org",
    "net",
    "html",
    "htm",
    "php",
    "aspx",
    "png",
    "jpg",
    "jpeg",
    "gif",
    "webp",
    "svg",
    "tmp",
    "var",
    "users",
    "home",
    "desktop",
    "documents",
    "downloads",
    "untitled",
    "screenshot",
    "screen",
    "shot",
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
    for (position, word) in title_words.iter().chain(content_words.iter()).enumerate() {
        let weight = if position < title_words.len() { 3 } else { 1 };
        *freq.entry(word.clone()).or_insert(0) += weight;
        first_seen.entry(word.clone()).or_insert(position);
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
    fn add_entry_query(
        id: String,
        content: String,
        title: String,
        source: String,
        created_at: i64,
    ) -> WriteBatch {
        write_batch()
            .var_as(
                "entry",
                g().add_n(
                    "Entry",
                    vec![
                        ("id", id),
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
    #[allow(unused_braces)]
    #[query]
    fn get_entry_query(id: String) -> ReadBatch {
        read_batch()
            .var_as("entry", g().n_where(SourcePredicate::eq("id", id)))
            .returning(["entry"])
    }
    #[allow(unused_braces)]
    #[query]
    fn list_entries_query(limit: i64) -> ReadBatch {
        read_batch()
            .var_as(
                "entries",
                g().n_with_label("Entry")
                    .order_by("created_at", Order::Desc)
                    .limit(limit),
            )
            .returning(["entries"])
    }
    #[query]
    fn search_entries_query(query: String, limit: i64) -> ReadBatch {
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
    pub fn add_entry_request(
        id: String,
        content: String,
        title: String,
        source: String,
        created_at: i64,
    ) -> Result<QueryRequest, QueryError> {
        add_entry_query(id, content, title, source, created_at)
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

    pub const HELIX_DEFAULT_URL: &str = "http://localhost:6969";

    #[derive(Clone, Debug, Serialize, Deserialize, Default)]
    struct Graph {
        entries: std::collections::HashMap<String, worktable_db::Entry>,
        entry_topics: std::collections::HashMap<String, Vec<String>>, // id -> topics
        topic_entries: std::collections::HashMap<String, Vec<String>>, // topic -> ids
        // For the embedded file we also store a simple relations cache
        relations: std::collections::HashMap<(String, String), f32>, // (id1,id2) -> jaccard
        /// Entries whose topics came from the AI enrichment pass. Kept so
        /// "Build knowledge" only asks the model about new entries.
        #[serde(default)]
        enriched: std::collections::HashSet<String>,
    }

    impl Graph {
        fn topics_for(&self, id: &str) -> Vec<String> {
            self.entry_topics.get(id).cloned().unwrap_or_default()
        }

        /// Refresh only the relations that involve `id`.
        ///
        /// The graph links entries whose topic sets overlap (Jaccard > 0.1).
        /// A full O(n²) rebuild on every synced entry made bulk imports
        /// stutter (O(n³) overall); touching one entry is O(n).
        fn refresh_relations_for(&mut self, id: &str) {
            self.relations
                .retain(|(a, b), _| a.as_str() != id && b.as_str() != id);
            let ta: std::collections::HashSet<String> = self.topics_for(id).into_iter().collect();
            if ta.is_empty() {
                return;
            }
            let others: Vec<String> = self
                .entries
                .keys()
                .filter(|other| other.as_str() != id)
                .cloned()
                .collect();
            for other in others {
                let tb: std::collections::HashSet<String> =
                    self.topics_for(&other).into_iter().collect();
                if tb.is_empty() {
                    continue;
                }
                let inter = ta.intersection(&tb).count() as f32;
                let uni = ta.union(&tb).count() as f32;
                let jaccard = inter / uni;
                if jaccard > 0.1 {
                    self.relations
                        .insert((id.to_owned(), other.clone()), jaccard);
                    self.relations.insert((other, id.to_owned()), jaccard);
                }
            }
        }

        /// Replace an entry's topics with the AI's (normalized) output and
        /// mark it enriched. Local extraction is a fallback, not a merge:
        /// the model's vocabulary is what search and relations should use.
        fn apply_ai_topics(&mut self, id: &str, topics: Vec<String>) -> bool {
            if !self.entries.contains_key(id) {
                return false;
            }
            let mut normalized: Vec<String> = topics
                .into_iter()
                .map(|topic| topic.trim().to_lowercase())
                .filter(|topic| !topic.is_empty() && topic.len() <= 80)
                .collect();
            normalized.dedup();
            normalized.truncate(8);
            if normalized.is_empty() {
                return false;
            }
            // Drop this entry from its old topics' reverse index.
            if let Some(old) = self.entry_topics.insert(id.to_owned(), normalized.clone()) {
                for topic in old {
                    if let Some(ids) = self.topic_entries.get_mut(&topic) {
                        ids.retain(|value| value != id);
                        if ids.is_empty() {
                            self.topic_entries.remove(&topic);
                        }
                    }
                }
            }
            for topic in &normalized {
                let ids = self.topic_entries.entry(topic.clone()).or_default();
                if !ids.contains(&id.to_owned()) {
                    ids.push(id.to_owned());
                    ids.sort();
                }
            }
            self.enriched.insert(id.to_owned());
            self.refresh_relations_for(id);
            true
        }

        /// Primary (first) topic per entry, uppercased for display.
        fn knowledge_topics(&self) -> std::collections::HashMap<String, String> {
            self.entry_topics
                .iter()
                .filter_map(|(id, topics)| {
                    topics
                        .first()
                        .map(|topic| (id.clone(), topic.to_uppercase()))
                })
                .collect()
        }

        /// Entries the AI has not enriched yet, bounded by `limit`.
        fn unenriched_ids(&self, limit: usize) -> Vec<String> {
            let mut ids: Vec<String> = self
                .entries
                .keys()
                .filter(|id| !self.enriched.contains(*id))
                .cloned()
                .collect();
            ids.sort();
            ids.truncate(limit);
            ids
        }

        /// Drop every relation that mentions `id` (entry removed).
        fn drop_relations_for(&mut self, id: &str) {
            self.relations
                .retain(|(a, b), _| a.as_str() != id && b.as_str() != id);
        }

        /// The strongest semantic neighbours of `id`, best first (max 5):
        /// `(entry_id, label)` pairs for the model and the UI.
        fn related_entries(&self, id: &str) -> Vec<(String, String)> {
            let mut related: Vec<(f32, String)> = self
                .relations
                .iter()
                .filter(|((a, _), _)| a == id)
                .map(|((_, b), score)| (*score, b.clone()))
                .collect();
            related.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.1.cmp(&b.1))
            });
            related
                .into_iter()
                .take(5)
                .map(|(_, other)| {
                    let label = self
                        .entries
                        .get(&other)
                        .map(|entry| match entry.title.as_deref() {
                            Some(title) if !title.trim().is_empty() => title.to_owned(),
                            _ => {
                                let text = entry
                                    .content
                                    .split_whitespace()
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                let mut excerpt: String = text.chars().take(60).collect();
                                if text.chars().count() > 60 {
                                    excerpt.push('…');
                                }
                                excerpt
                            }
                        })
                        .unwrap_or_else(|| other.clone());
                    (other, label)
                })
                .collect()
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
            let url =
                url.unwrap_or_else(|| super::default_helix_path().to_string_lossy().to_string());
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
            let sqlite_path = std::env::var("WORKTABLE_DB_PATH")
                .ok()
                .or_else(|| {
                    std::env::var("HOME")
                        .ok()
                        .map(|h| format!("{}/.worktable/worktable.db", h))
                })
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
            // Load existing file if present.
            if let Ok(data) = std::fs::read_to_string(&path)
                && let Ok(g) = serde_json::from_str::<Graph>(&data)
            {
                *client.graph.lock().unwrap() = g;
            }
            client
        }
        pub fn url(&self) -> &str {
            &self.http_url
        }
        pub fn has_client(&self) -> bool {
            // The embedded client is backed by the in-process graph file.
            true
        }
        fn save(&self) {
            if let Some(parent) = self.path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(data) = serde_json::to_string_pretty(&*self.graph.lock().unwrap()) {
                let _ = std::fs::write(&self.path, data);
            }
        }
        pub async fn is_available(&self) -> bool {
            // The embedded graph needs no network service; reads and writes
            // are local to the graph file.
            true
        }
        pub fn is_available_blocking(&self) -> bool {
            true
        }
        pub async fn sync_entry(&self, entry: &worktable_db::Entry) -> anyhow::Result<()> {
            let topics = super::topic_for_entry(entry);
            let mut g = self.graph.lock().unwrap();
            g.entries.insert(entry.id.clone(), entry.clone());
            g.entry_topics.insert(entry.id.clone(), topics.clone());
            for t in &topics {
                g.topic_entries
                    .entry(t.clone())
                    .or_default()
                    .push(entry.id.clone());
                // dedup
                let v = g.topic_entries.get_mut(t).unwrap();
                v.sort();
                v.dedup();
            }
            g.refresh_relations_for(&entry.id);
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
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let res = rt.block_on(client.sync_entry(&entry));
                    let _ = tx.send(res);
                });
                rx.recv().unwrap()
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(self.sync_entry(entry))
            }
        }
        /// Apply AI-extracted topics to one entry and persist the graph.
        pub async fn apply_ai_topics(&self, id: &str, topics: Vec<String>) -> anyhow::Result<bool> {
            let changed = {
                let mut graph = self.graph.lock().unwrap();
                graph.apply_ai_topics(id, topics)
            };
            if changed {
                self.save();
            }
            Ok(changed)
        }

        /// Blocking form of [`Self::apply_ai_topics`] (GPUI threads).
        pub fn apply_ai_topics_blocking(
            &self,
            id: &str,
            topics: Vec<String>,
        ) -> anyhow::Result<bool> {
            let changed = {
                let mut graph = self.graph.lock().unwrap();
                graph.apply_ai_topics(id, topics)
            };
            if changed {
                self.save();
            }
            Ok(changed)
        }

        /// Entries that still need AI topic extraction (from SQLite order).
        pub fn unenriched_entries_blocking(
            &self,
            sqlite_path: &str,
            limit: usize,
        ) -> anyhow::Result<Vec<worktable_db::Entry>> {
            let ids = {
                let graph = self.graph.lock().unwrap();
                graph.unenriched_ids(limit)
            };
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let store = worktable_db::SqliteStore::connect(sqlite_path)?;
            store.migrate()?;
            let by_id: std::collections::HashMap<String, worktable_db::Entry> = store
                .list_entries(5000)?
                .into_iter()
                .map(|entry| (entry.id.clone(), entry))
                .collect();
            Ok(ids
                .into_iter()
                .filter_map(|id| by_id.get(&id).cloned())
                .collect())
        }

        /// Primary topic per entry, for the UI's topic cache and grouping.
        pub fn knowledge_topics_blocking(&self) -> std::collections::HashMap<String, String> {
            self.graph.lock().unwrap().knowledge_topics()
        }

        pub async fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
            let mut g = self.graph.lock().unwrap();
            g.entries.remove(id);
            if let Some(topics) = g.entry_topics.remove(id) {
                for t in topics {
                    if let Some(v) = g.topic_entries.get_mut(&t) {
                        v.retain(|x| x != id);
                        if v.is_empty() {
                            g.topic_entries.remove(&t);
                        }
                    }
                }
            }
            g.drop_relations_for(id);
            drop(g);
            self.save();
            Ok(())
        }
        pub fn delete_entry_blocking(&self, id: &str) -> anyhow::Result<()> {
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.clone();
                let id = id.to_owned();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let res = rt.block_on(client.delete_entry(&id));
                    let _ = tx.send(res);
                });
                rx.recv().unwrap()
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(self.delete_entry(id))
            }
        }
        pub async fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            let q = query.to_lowercase();
            let g = self.graph.lock().unwrap();
            let mut scored: Vec<(f32, &worktable_db::Entry)> = Vec::new();
            for entry in g.entries.values() {
                let hay = format!(
                    "{} {} {}",
                    entry.title.clone().unwrap_or_default(),
                    entry.content,
                    entry.source
                )
                .to_lowercase();
                let mut score = 0.0;
                // Direct substring hit against title/content/source.
                if hay.contains(&q) {
                    score += 10.0;
                }
                // Topic boost: only when the *query* relates to one of the
                // entry's topics. (Boosting for `hay.contains(topic)` would
                // score every entry on every query, since an entry's own
                // topics always appear in its text.)
                for t in g.topics_for(&entry.id) {
                    if q.contains(&t) || t.contains(&q) {
                        score += 5.0;
                    }
                }
                if score > 0.0 {
                    scored.push((score, entry));
                }
            }
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
            let out: Vec<JsonValue> = scored
                .into_iter()
                .take(limit)
                .map(|(_, e)| {
                    let mut value = serde_json::to_value(e).unwrap_or(JsonValue::Null);
                    let related: Vec<JsonValue> = g
                        .related_entries(&e.id)
                        .into_iter()
                        .map(|(id, label)| serde_json::json!({ "id": id, "label": label }))
                        .collect();
                    if let Some(object) = value.as_object_mut() {
                        object.insert("related".to_owned(), JsonValue::Array(related));
                    }
                    value
                })
                .collect();
            Ok(out)
        }
        pub async fn search_best_effort(
            &self,
            query: &str,
            limit: usize,
        ) -> anyhow::Result<Vec<JsonValue>> {
            Ok(self.search(query, limit).await.unwrap_or_default())
        }
        pub fn search_blocking(&self, query: &str, limit: usize) -> anyhow::Result<Vec<JsonValue>> {
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.clone();
                let q = query.to_owned();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let res = rt.block_on(client.search(&q, limit));
                    let _ = tx.send(res);
                });
                rx.recv().unwrap()
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
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
            for entry in entries {
                // Re-sync entries whose graph copy is missing or stale (an
                // import may have added a description after the first mirror,
                // and edits must reach the graph too). Changed entries lose
                // their AI enrichment so the next pass re-names them.
                let needs_sync = {
                    let mut graph = self.graph.lock().unwrap();
                    match graph.entries.get(&entry.id) {
                        None => true,
                        Some(existing)
                            if existing.content != entry.content
                                || existing.title != entry.title
                                || existing.source != entry.source =>
                        {
                            graph.enriched.remove(&entry.id);
                            true
                        }
                        Some(_) => false,
                    }
                };
                if needs_sync {
                    self.sync_entry(&entry).await?;
                    synced += 1;
                }
            }
            // Topics and relations are refreshed per synced entry.
            Ok(synced)
        }
        pub fn build_from_sqlite_blocking(&self, sqlite_path: &str) -> anyhow::Result<usize> {
            if tokio::runtime::Handle::try_current().is_ok() {
                let client = self.clone();
                let path = sqlite_path.to_owned();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    let res = rt.block_on(client.build_from_sqlite(&path));
                    let _ = tx.send(res);
                });
                rx.recv().unwrap()
            } else {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
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
        pub fn new(_url: Option<String>) -> Self {
            Self
        }
        pub fn from_env() -> Self {
            Self
        }
        pub fn url(&self) -> &str {
            "wasm-stub"
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
        pub async fn sync_entry(&self, _entry: &Entry) -> anyhow::Result<()> {
            Ok(())
        }
        pub fn sync_entry_blocking(&self, _entry: &Entry) -> anyhow::Result<()> {
            Ok(())
        }
        pub async fn delete_entry(&self, _id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        pub fn delete_entry_blocking(&self, _id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        pub async fn search(
            &self,
            _query: &str,
            _limit: usize,
        ) -> anyhow::Result<Vec<serde_json::Value>> {
            Ok(vec![])
        }
        pub async fn search_best_effort(
            &self,
            _q: &str,
            _l: usize,
        ) -> anyhow::Result<Vec<serde_json::Value>> {
            Ok(vec![])
        }
        pub fn search_blocking(
            &self,
            _q: &str,
            _l: usize,
        ) -> anyhow::Result<Vec<serde_json::Value>> {
            Ok(vec![])
        }
        pub fn search_best_effort_blocking(&self, _q: &str, _l: usize) -> Vec<serde_json::Value> {
            vec![]
        }
        pub async fn build_from_sqlite(&self, _path: &str) -> anyhow::Result<usize> {
            Ok(0)
        }
        pub fn build_from_sqlite_blocking(&self, _path: &str) -> anyhow::Result<usize> {
            Ok(0)
        }
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
            "this", "that", "with", "from", "2024", "2025", "tmp", "photo", "png", "https", "com",
            "html",
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

    /// A build re-syncs entries whose stored content changed (an import can
    /// add a description after the first mirror) and drops their AI topics so
    /// the next enrichment pass re-names them.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn build_resyncs_changed_entries_and_invalidates_enrichment() {
        let dir = std::env::temp_dir().join(format!("helix-test-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("worktable.db");
        let db = db_path.to_string_lossy().into_owned();
        let store = worktable_db::SqliteStore::connect(&db).unwrap();
        store.migrate().unwrap();

        let mut first = entry(None, "a star with no description yet");
        first.id = "e1".to_owned();
        store.insert_entry(&first).unwrap();

        let client = HelixClient::open_embedded(dir.join("helix.json"));
        assert_eq!(client.build_from_sqlite_blocking(&db).unwrap(), 1);
        client
            .apply_ai_topics_blocking("e1", vec!["stale".to_owned()])
            .unwrap();

        // The GitHub import fills the description in afterwards.
        store
            .update_entry_content(
                "e1",
                "Fast, productive tooling for building native applications",
            )
            .unwrap();
        assert_eq!(
            client.build_from_sqlite_blocking(&db).unwrap(),
            1,
            "changed content re-syncs"
        );
        let hits = client.search_blocking("productive tooling", 10).unwrap();
        assert!(
            !hits.is_empty(),
            "the description is searchable after the rebuild"
        );
        let pending = client.unenriched_entries_blocking(&db, 10).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "changed entries are queued for AI naming again"
        );
    }

    /// AI topics replace the local extractor's, mark the entry enriched, and
    /// drive relations; un-enriched entries are what the next build asks the
    /// model about.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn ai_topics_mark_enrichment_and_link_entries() {
        let dir = std::env::temp_dir().join(format!("helix-test-{}", uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("worktable.db");
        let db = db_path.to_string_lossy().into_owned();
        let store = worktable_db::SqliteStore::connect(&db).unwrap();
        store.migrate().unwrap();

        let mut first = entry(None, "a plain note that the model will describe");
        first.id = "e1".to_owned();
        let mut second = entry(None, "another plain note with no shared words");
        second.id = "e2".to_owned();
        store.insert_entry(&first).unwrap();
        store.insert_entry(&second).unwrap();

        let client = HelixClient::open_embedded(dir.join("helix.json"));
        client.sync_entry_blocking(&first).unwrap();
        client.sync_entry_blocking(&second).unwrap();

        let pending = client.unenriched_entries_blocking(&db, 10).unwrap();
        assert_eq!(pending.len(), 2, "both entries start un-enriched");

        client
            .apply_ai_topics_blocking("e1", vec!["Sunsets".to_owned(), "mountains".to_owned()])
            .unwrap();
        let pending = client.unenriched_entries_blocking(&db, 10).unwrap();
        assert_eq!(pending.len(), 1, "enriched entries drop out of the queue");
        assert_eq!(pending[0].id, "e2");

        let topics = client.knowledge_topics_blocking();
        assert_eq!(
            topics.get("e1").map(String::as_str),
            Some("SUNSETS"),
            "the primary topic is exported for the UI"
        );

        // The model links the two notes by topic even though their words do
        // not overlap.
        client
            .apply_ai_topics_blocking("e2", vec!["sunsets".to_owned(), "photography".to_owned()])
            .unwrap();
        let hits = client.search_blocking("sunsets", 10).unwrap();
        let related: Vec<&str> = hits
            .iter()
            .flat_map(|hit| {
                hit.get("related")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
            })
            .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
            .collect();
        assert!(
            related.contains(&"e1") || related.contains(&"e2"),
            "AI topics create the semantic link: {related:?}"
        );
    }

    /// Entries whose topic sets overlap are linked, and the links ride along
    /// with search results so one hit surfaces its semantic neighbours.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn related_entries_are_linked_and_surfaced_in_search() {
        let dir = std::env::temp_dir().join(format!("helix-test-{}", uuid_v4()));
        let path = dir.join("helix.json");
        let client = HelixClient::open_embedded(path);

        let mut mountains = entry(None, "sunset mountains breathtaking alpine photography");
        mountains.id = "mountains".to_owned();
        let mut desert = entry(None, "sunset desert photography dunes breathtaking");
        desert.id = "desert".to_owned();
        let mut unrelated = entry(None, "parser internals compiler lexer grammar");
        unrelated.id = "parser".to_owned();

        client.sync_entry_blocking(&mountains).unwrap();
        client.sync_entry_blocking(&desert).unwrap();
        client.sync_entry_blocking(&unrelated).unwrap();

        let hits = client.search_blocking("sunset", 10).unwrap();
        assert!(!hits.is_empty(), "the shared topic matches both entries");
        let related: Vec<&str> = hits
            .iter()
            .flat_map(|hit| {
                hit.get("related")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
            })
            .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
            .collect();
        assert!(
            related.contains(&"desert") || related.contains(&"mountains"),
            "overlapping topics link the sunset notes: {related:?}"
        );
        assert!(
            !related.contains(&"parser"),
            "an unrelated entry must not be linked: {related:?}"
        );

        // Removing an entry drops its links.
        client.delete_entry_blocking("desert").unwrap();
        let hits = client.search_blocking("sunset", 10).unwrap();
        let related: Vec<&str> = hits
            .iter()
            .flat_map(|hit| {
                hit.get("related")
                    .and_then(|value| value.as_array())
                    .into_iter()
                    .flatten()
            })
            .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
            .collect();
        assert!(
            !related.contains(&"desert"),
            "deleted entries leave no dangling links: {related:?}"
        );
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
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        // Parallel tests share the process clock; the counter guarantees a
        // unique temp path even when two calls land in the same nanosecond.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{}-{}-{}", std::process::id(), nanos, count)
    }
}
