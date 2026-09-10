//! Helix knowledge search tool for the rig agent.
//!
//! This module is `#[cfg(not(target_arch = "wasm32"))]` only — HelixDB's
//! `reqwest` + `tokio` stack does not target `wasm32-unknown-unknown`.
//! On WASM the agent stays chat-only.
//!
//! The tool exposes a single function, `search_knowledge`, to the LLM so it
//! can retrieve `Entry` nodes mirrored into Helix's graph. SQLite remains the
//! source of truth; Helix is best-effort and the tool degrades gracefully
//! when the graph is empty.
//!
//! Each call numbers its hits `[1]..[n]` (continuing across calls in one run)
//! so the model can cite them; the numbered hits are also recorded in a
//! [`CitationCollector`] the agent runtime drains into the UI.

#[cfg(not(target_arch = "wasm32"))]
mod native_impl {
    use std::sync::{Arc, Mutex};

    use rig::tool::{Tool, ToolContext};
    use serde::Deserialize;
    use serde_json::{Value as JsonValue, json};
    use worktable_helix::HelixClient;

    use crate::worker_protocol::KnowledgeCitation;

    /// A minimal tool error: rig's `Tool::Error` requires a concrete
    /// `std::error::Error` type, and `anyhow::Error` no longer implements it.
    #[derive(Debug)]
    pub struct ToolError(String);

    impl std::fmt::Display for ToolError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl std::error::Error for ToolError {}

    /// Arguments accepted by `search_knowledge`.
    #[derive(Debug, Deserialize)]
    pub struct SearchKnowledgeArgs {
        /// Substring to search for in entry content and title.
        pub query: String,
        /// Maximum entries to return (clamped to 1..=100, default 10).
        #[serde(default)]
        pub limit: Option<usize>,
    }

    /// Collects the citations produced by `search_knowledge` during one run.
    ///
    /// Numbering continues across calls so two searches in a single answer
    /// never reuse a marker for different sources.
    #[derive(Clone, Default, Debug)]
    pub struct CitationCollector {
        inner: Arc<Mutex<Vec<KnowledgeCitation>>>,
    }

    impl CitationCollector {
        pub fn new() -> Self {
            Self::default()
        }

        /// Number `items` after everything already collected, store them, and
        /// return the numbered copies.
        pub fn push_all(&self, items: Vec<KnowledgeCitation>) -> Vec<KnowledgeCitation> {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut numbered = Vec::with_capacity(items.len());
            for mut item in items {
                item.n = inner.len() as u32 + 1;
                inner.push(item.clone());
                numbered.push(item);
            }
            numbered
        }

        /// Whether anything has been collected yet.
        pub fn is_empty(&self) -> bool {
            self.inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        }

        /// Drain the collected citations.
        pub fn take(&self) -> Vec<KnowledgeCitation> {
            std::mem::take(
                &mut *self
                    .inner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }
    }

    /// The `search_knowledge` tool over the embedded Helix graph.
    ///
    /// The graph file is opened per call: a client constructed once would
    /// cache the file as it was when the agent was built, so entries mirrored
    /// (or imported) after that point would stay invisible for the whole run.
    #[derive(Debug, Clone)]
    pub struct SearchKnowledgeTool {
        url: Option<String>,
        citations: CitationCollector,
    }

    impl SearchKnowledgeTool {
        pub fn new() -> Self {
            Self {
                url: None,
                citations: CitationCollector::new(),
            }
        }

        pub fn with_url(url: Option<String>) -> Self {
            Self {
                url,
                citations: CitationCollector::new(),
            }
        }

        /// Record hits into `citations` (shared with the agent runtime).
        pub fn with_citations(mut self, citations: CitationCollector) -> Self {
            self.citations = citations;
            self
        }

        fn open_graph(&self) -> HelixClient {
            match &self.url {
                Some(url) => HelixClient::new(Some(url.clone())),
                None => HelixClient::from_env(),
            }
        }
    }

    impl Default for SearchKnowledgeTool {
        fn default() -> Self {
            Self::new()
        }
    }

    /// A human label for a hit: its title, or a short content excerpt.
    fn hit_label(title: &str, content: &str) -> String {
        let title = title.trim();
        if !title.is_empty() {
            return title.to_owned();
        }
        excerpt(content, 60)
    }

    /// A one-line content excerpt, truncated on a character boundary.
    fn excerpt(content: &str, max_chars: usize) -> String {
        let text = content.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut out: String = text.chars().take(max_chars).collect();
        if text.chars().count() > max_chars {
            out.push('…');
        }
        out
    }

    /// If the content starts with an http(s) URL, return `(url, host)`.
    fn external_url(content: &str) -> Option<(String, String)> {
        let first = content.split_whitespace().next()?.trim();
        let rest = first
            .strip_prefix("https://")
            .or_else(|| first.strip_prefix("http://"))?;
        let host = rest.split(['/', '?', '#']).next()?.trim();
        if host.is_empty() {
            return None;
        }
        Some((first.to_owned(), host.to_owned()))
    }

    /// Build a citable reference for one search hit.
    fn citation_for_hit(hit: &JsonValue) -> KnowledgeCitation {
        let entry_id = hit
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_owned();
        let title = hit
            .get("title")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let content = hit
            .get("content")
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        let source = hit
            .get("source")
            .and_then(JsonValue::as_str)
            .unwrap_or("Worktable");
        let (url, host) =
            external_url(content).unwrap_or_else(|| (String::new(), source.to_owned()));
        KnowledgeCitation {
            n: 0,
            entry_id,
            label: hit_label(title, content),
            snippet: excerpt(content, 160).replace('\n', " "),
            host,
            url,
        }
    }

    /// Render the numbered payload the model reads. Related entries are
    /// included so one search surfaces the semantic neighbourhood.
    fn render_hits(hits: &[JsonValue], numbered: &[KnowledgeCitation], query: &str) -> String {
        let blocks: Vec<String> = hits
            .iter()
            .zip(numbered)
            .map(|(hit, citation)| {
                let content = hit
                    .get("content")
                    .and_then(JsonValue::as_str)
                    .unwrap_or_default();
                let source = hit
                    .get("source")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("Worktable");
                let related: Vec<String> = hit
                    .get("related")
                    .and_then(JsonValue::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                let id = item.get("id")?.as_str()?;
                                let label =
                                    item.get("label").and_then(JsonValue::as_str).unwrap_or(id);
                                Some(format!("{id} ({label})"))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let mut block = format!(
                    "[{}] {}\n    id: {} · source: {source}\n    {}",
                    citation.n,
                    citation.label,
                    citation.entry_id,
                    excerpt(content, 400)
                );
                if !related.is_empty() {
                    block.push_str(&format!("\n    related: {}", related.join(", ")));
                }
                block
            })
            .collect();
        let count = blocks.len();
        format!(
            "search_knowledge: {count} entr{} for {query:?}. Cite the entries you use with \
             their [n] markers.\n\n{}",
            if count == 1 { "y" } else { "ies" },
            blocks.join("\n\n")
        )
    }

    impl Tool for SearchKnowledgeTool {
        const NAME: &'static str = "search_knowledge";

        type Args = SearchKnowledgeArgs;
        type Output = String;
        type Error = ToolError;

        fn description(&self) -> String {
            "Search the Worktable knowledge base (topics and semantic links built from the \
             user's saved entries) for entries matching a query. Use when you need context \
             about what the user has saved. Returns numbered entries with their id, \
             content, title, source, created_at, and semantically related entries. Cite \
             used entries as [n]."
                .to_owned()
        }

        fn parameters(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Substring to search for in entry content and title"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum entries to return (1-100, default 10)",
                        "minimum": 1,
                        "maximum": 100,
                        "default": 10
                    }
                },
                "required": ["query"]
            })
        }

        async fn call(
            &self,
            _context: &mut ToolContext,
            args: Self::Args,
        ) -> Result<Self::Output, Self::Error> {
            let query = args.query.trim();
            if query.is_empty() {
                return Err(ToolError(
                    "search_knowledge: query must not be empty".to_owned(),
                ));
            }
            let limit = args.limit.unwrap_or(10).clamp(1, 100);

            // Use the async search: the tool runs on Tokio, and the blocking
            // variant would park a worker thread. Re-open the graph so the
            // search sees every entry mirrored up to this call.
            let helix = self.open_graph();
            let hits = helix
                .search_best_effort(query, limit)
                .await
                .unwrap_or_default();
            if hits.is_empty() {
                return Ok(format!(
                    "search_knowledge: no entries matched {query:?} (limit {limit}). \
                     Try a broader query."
                ));
            }

            let citations: Vec<KnowledgeCitation> = hits.iter().map(citation_for_hit).collect();
            let numbered = self.citations.push_all(citations);
            Ok(render_hits(&hits, &numbered, query))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use worktable_db::Entry;

        fn block_on<F: std::future::Future>(future: F) -> F::Output {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(future)
        }

        fn temp_graph() -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("wt-tool-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).expect("temp dir");
            dir.join("helix.json")
        }

        fn entry(id: &str, content: &str, title: Option<&str>) -> Entry {
            Entry {
                id: id.to_owned(),
                content: content.to_owned(),
                title: title.map(str::to_owned),
                source: "Worktable".to_owned(),
                created_at: 1_000,
            }
        }

        #[test]
        fn search_returns_numbered_citations_and_finishes() {
            let path = temp_graph();
            let client = HelixClient::open_embedded(path.clone());
            block_on(client.sync_entry(&entry(
                "e1",
                "The sunset over the mountains was breathtaking",
                None,
            )))
            .expect("seed graph");

            let collector = CitationCollector::new();
            let tool = SearchKnowledgeTool::with_url(Some(path.to_string_lossy().into_owned()))
                .with_citations(collector.clone());
            let mut context = ToolContext::new();

            let output = block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tool.call(
                        &mut context,
                        SearchKnowledgeArgs {
                            query: "sunset".to_owned(),
                            limit: Some(10),
                        },
                    ),
                )
                .await
            })
            .expect("the tool must finish, not hang")
            .expect("tool call succeeds");

            assert!(output.contains("[1]"), "output numbers its hits: {output}");
            let citations = collector.take();
            assert_eq!(citations.len(), 1, "one citation collected");
            assert_eq!(citations[0].n, 1);
            assert_eq!(citations[0].entry_id, "e1");
            assert!(
                citations[0].label.contains("sunset"),
                "label falls back to the content excerpt: {:?}",
                citations[0].label
            );
        }

        #[test]
        fn search_without_hits_explains_and_finishes() {
            let path = temp_graph();
            // A graph file that has never been written: the tool must still
            // answer, not wait for a service.
            let collector = CitationCollector::new();
            let tool = SearchKnowledgeTool::with_url(Some(path.to_string_lossy().into_owned()))
                .with_citations(collector.clone());
            let mut context = ToolContext::new();

            let output = block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tool.call(
                        &mut context,
                        SearchKnowledgeArgs {
                            query: "nothing-here".to_owned(),
                            limit: None,
                        },
                    ),
                )
                .await
            })
            .expect("the tool must finish, not hang")
            .expect("tool call succeeds");

            assert!(output.contains("no entries matched"), "got: {output}");
            assert!(collector.is_empty(), "no citations for an empty search");
        }

        #[test]
        fn citation_numbers_continue_across_calls() {
            let collector = CitationCollector::new();
            let first = collector.push_all(vec![KnowledgeCitation {
                n: 0,
                entry_id: "a".to_owned(),
                label: "A".to_owned(),
                snippet: String::new(),
                host: "Worktable".to_owned(),
                url: String::new(),
            }]);
            let second = collector.push_all(vec![KnowledgeCitation {
                n: 0,
                entry_id: "b".to_owned(),
                label: "B".to_owned(),
                snippet: String::new(),
                host: "Worktable".to_owned(),
                url: String::new(),
            }]);
            assert_eq!(first[0].n, 1);
            assert_eq!(second[0].n, 2, "markers never repeat within a run");
            assert_eq!(collector.take().len(), 2);
        }

        #[test]
        fn external_urls_split_into_url_and_host() {
            let (url, host) = external_url("https://arxiv.org/abs/1706.03762").expect("url entry");
            assert_eq!(url, "https://arxiv.org/abs/1706.03762");
            assert_eq!(host, "arxiv.org");
            assert!(external_url("just a note").is_none());
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native_impl::{CitationCollector, SearchKnowledgeArgs, SearchKnowledgeTool, ToolError};
