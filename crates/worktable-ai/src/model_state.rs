//! Persist only API-discovered models, never credentials or guessed catalogs.

use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use worktable_db::SqliteStore;
use worktable_events::ModelInfo;

use crate::model_catalog::FetchedModels;

/// Injectable boundary for hermetic tests; production uses the authenticated API.
pub type ModelLoader = Arc<
    dyn Fn(SqliteStore, String) -> BoxFuture<'static, Result<FetchedModels, String>> + Send + Sync,
>;

/// Bump whenever cached rows could carry semantics this build no longer
/// accepts (for example rows without endpoint metadata or Zen free models).
const CACHE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
pub(crate) struct ModelState {
    /// Version of the row semantics, not of the provider's catalog.
    #[serde(default)]
    pub version: u32,
    pub revision: u64,
    pub models: Vec<ModelInfo>,
    pub loading: bool,
    pub error: Option<String>,
}

impl Default for ModelState {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            revision: 0,
            models: Vec::new(),
            loading: false,
            error: None,
        }
    }
}

fn key(provider_id: &str) -> String {
    format!("provider_models:{provider_id}")
}

pub(crate) fn read(store: &SqliteStore, provider_id: &str) -> ModelState {
    let state = store
        .get_config(&key(provider_id))
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<ModelState>(&json).ok());
    match state {
        Some(state) if state.version == CACHE_VERSION => state,
        _ => ModelState::default(),
    }
}

pub(crate) fn write(
    store: &SqliteStore,
    provider_id: &str,
    state: &ModelState,
) -> anyhow::Result<()> {
    store.set_config(&key(provider_id), &serde_json::to_string(state)?)
}

pub(crate) fn invalidate(store: &SqliteStore, provider_id: &str) -> anyhow::Result<()> {
    let revision = read(store, provider_id).revision.wrapping_add(1);
    write(
        store,
        provider_id,
        &ModelState {
            revision,
            ..ModelState::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_credentials_discards_only_that_providers_fetched_catalog() {
        let store = SqliteStore::connect(":memory:").unwrap();
        store.migrate().unwrap();
        let state = ModelState {
            revision: 7,
            models: vec![ModelInfo {
                id: "server-model".into(),
                name: "Server model".into(),
                group: None,
                api: None,
                free: false,
            }],
            ..Default::default()
        };
        write(&store, "a", &state).unwrap();
        write(&store, "b", &state).unwrap();
        invalidate(&store, "a").unwrap();
        assert_eq!(read(&store, "a").revision, 8);
        assert!(read(&store, "a").models.is_empty());
        assert_eq!(read(&store, "b").models[0].id, "server-model");
    }

    #[test]
    fn old_cache_without_metadata_version_is_ignored() {
        let store = SqliteStore::connect(":memory:").unwrap();
        store.migrate().unwrap();
        store
            .set_config(
                &key("opencode-go"),
                r#"{"revision":4,"models":[{"id":"big-pickle","name":"Big Pickle","group":"OpenCode Zen"}],"loading":false,"error":null}"#,
            )
            .unwrap();
        let state = read(&store, "opencode-go");
        assert_eq!(state.version, CACHE_VERSION);
        assert_eq!(state.revision, 0);
        assert!(
            state.models.is_empty(),
            "pre-metadata rows must not survive the cache version bump"
        );
        assert!(!state.loading);
        assert!(state.error.is_none());
    }
}
