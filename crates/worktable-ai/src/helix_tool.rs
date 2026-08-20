//! Helix knowledge search tool for the embedded Pi agent.
//!
//! This module is `#[cfg(not(target_arch = "wasm32"))]` only — HelixDB's
//! `reqwest` + `tokio` stack does not target `wasm32-unknown-unknown`.
//! On WASM the agent stays chat-only.
//!
//! The tool exposes a single function `search_knowledge` to the LLM so it can
//! retrieve `Entry` nodes mirrored into Helix's graph. SQLite remains the
//! source-of-truth; Helix is best-effort and the tool gracefully degrades when
//! the gateway (`http://localhost:6969`) is not running.

#[cfg(not(target_arch = "wasm32"))]
mod native_impl {
    use std::path::Path;

    use async_trait::async_trait;
    use pi::config::Config;
    use pi::model::{ContentBlock, TextContent};
    use pi::sdk::{default_tool_registry, ToolFactory};
    use pi::tools::{Tool, ToolEffects, ToolOutput, ToolRegistry, ToolUpdate};
    use serde::Deserialize;
    use worktable_helix::HelixClient;

    // -------------------------------------------------------------------------
    // SearchKnowledgeTool
    // -------------------------------------------------------------------------

    #[derive(Debug, Clone)]
    pub struct SearchKnowledgeTool {
        helix: HelixClient,
    }

    impl SearchKnowledgeTool {
        pub fn new() -> Self {
            Self {
                helix: HelixClient::from_env(),
            }
        }

        pub fn with_url(url: Option<String>) -> Self {
            Self {
                helix: HelixClient::new(url),
            }
        }
    }

    impl Default for SearchKnowledgeTool {
        fn default() -> Self {
            Self::new()
        }
    }

    #[derive(Debug, Deserialize)]
    struct SearchKnowledgeInput {
        query: String,
        #[serde(default)]
        limit: Option<usize>,
    }

    #[async_trait]
    impl Tool for SearchKnowledgeTool {
        fn name(&self) -> &str {
            "search_knowledge"
        }

        fn label(&self) -> &str {
            "search_knowledge"
        }

        fn description(&self) -> &str {
            "Search the Worktable knowledge base (Helix graph + SQLite fallback) for entries matching a query. Use when you need context about saved notes, links, or images. Args: query (string, required), limit (integer, default 10). Returns JSON array of Entry objects with id, kind, content, title, source, created_at. Mirrors the user's saved Worktable entries via HelixDB; when Helix is unavailable returns an explanatory message."
        }

        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({
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

        fn effects(&self) -> ToolEffects {
            ToolEffects::read()
        }

        async fn execute(
            &self,
            _tool_call_id: &str,
            input: serde_json::Value,
            _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
        ) -> Result<ToolOutput, pi::error::Error> {
            let input: SearchKnowledgeInput = serde_json::from_value(input)
                .map_err(|e| pi::error::Error::validation(format!("invalid search_knowledge args: {e}")))?;

            let query = input.query.trim();
            if query.is_empty() {
                return Err(pi::error::Error::validation(
                    "search_knowledge: query must not be empty".to_string(),
                ));
            }
            let limit = input.limit.unwrap_or(10).clamp(1, 100);

            // HelixClient::search_blocking builds its own tokio runtime, so this
            // works on pi's asupersync executor without needing a tokio context.
            let helix_hits = self.helix.search_best_effort_blocking(query, limit);

            // Also surface a fallback note when Helix returned nothing but may be
            // down — the host can synthesize SQLite results if desired.
            let text = if helix_hits.is_empty() {
                // Probe availability to craft a helpful message.
                let available = self.helix.is_available_blocking();
                if !available {
                    format!(
                        "search_knowledge: HelixDB not available at {} — no Helix results for {:?}. \
                         The app's SQLite store (~/.worktable/worktable.db or $WORKTABLE_DB_PATH) remains the source of truth; \
                         Helix is a parallel graph that mirrors entries via `HelixClient::sync_entry`. \
                         If you need results now, suggest querying SQLite or retrying after `helix start dev`.",
                        self.helix.url(),
                        query
                    )
                } else {
                    format!(
                        "search_knowledge: no Helix entries matched {:?} (limit {limit}). \
                         Helix gateway {} is reachable but returned 0 hits. \
                         Try a broader query or check `list_entries`.",
                        query,
                        self.helix.url()
                    )
                }
            } else {
                let payload = serde_json::to_string_pretty(&helix_hits).unwrap_or_else(|_| format!("{helix_hits:?}"));
                format!(
                    "search_knowledge: {n} Helix hit(s) for {query:?} (limit {limit}) — gateway {}:\n{payload}",
                    self.helix.url(),
                    n = helix_hits.len()
                )
            };

            Ok(ToolOutput {
                content: vec![ContentBlock::Text(TextContent::new(text))],
                details: Some(serde_json::json!({
                    "helix_url": self.helix.url(),
                    "query": query,
                    "limit": limit,
                    "hits": helix_hits,
                })),
                is_error: false,
            })
        }
    }

    // -------------------------------------------------------------------------
    // HelixToolFactory — plugs search_knowledge into Pi's ToolRegistry
    // -------------------------------------------------------------------------

    /// Tool factory that extends Pi's built-in registry with `search_knowledge`.
    ///
    /// Pi calls this once per session during `create_agent_session`. We start
    /// from the default registry (honoring `enabled_tools` as usual) and layer
    /// our Helix tool on top so the model can discover it. When Helix is not
    /// running the tool still registers but its `execute` returns a fallback
    /// message rather than erroring the whole run.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct HelixToolFactory;

    impl ToolFactory for HelixToolFactory {
        fn create_tool_registry(
            &self,
            enabled: &[&str],
            cwd: &Path,
            config: &Config,
        ) -> ToolRegistry {
            let mut registry = default_tool_registry(enabled, cwd, config);
            // Always layer search_knowledge on top — dedup if the caller already
            // asked for it via `enabled_tools` (default registry would have
            // silently ignored the unknown name).
            if registry.get("search_knowledge").is_none() {
                registry.push(Box::new(SearchKnowledgeTool::new()));
            }
            registry
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native_impl::{HelixToolFactory, SearchKnowledgeTool};
