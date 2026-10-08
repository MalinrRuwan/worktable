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

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct ModelState {
    pub revision: u64,
    pub models: Vec<ModelInfo>,
    pub loading: bool,
    pub error: Option<String>,
}

fn key(provider_id: &str) -> String {
    format!("provider_models:{provider_id}")
}

pub(crate) fn read(store: &SqliteStore, provider_id: &str) -> ModelState {
    store
        .get_config(&key(provider_id))
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
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
            }],
            loading: false,
            error: None,
        };
        write(&store, "a", &state).unwrap();
        write(&store, "b", &state).unwrap();
        invalidate(&store, "a").unwrap();
        assert_eq!(read(&store, "a").revision, 8);
        assert!(read(&store, "a").models.is_empty());
        assert_eq!(read(&store, "b").models[0].id, "server-model");
    }
}
