//! Bridge between the GPUI main thread and the async `WorktableRuntime`.
//!
//! All database work runs on a dedicated Tokio runtime so the UI never blocks.
//! When the local SQLite database cannot be opened the service transparently
//! falls back to an in-memory store so the app stays fully usable.

use std::{
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;
use worktable_ai::{WorktableEntry, WorktableRuntime};
use worktable_events::WorktableEvent;

pub type EntryResult = Result<WorktableEntry, String>;
pub type ListResult = Result<Vec<WorktableEntry>, String>;
pub type EmptyResult = Result<(), String>;

/// Commands the macOS menu-bar status item can send into the app.
#[derive(Debug, Clone)]
pub enum AppCommand {
    ToggleWindow,
    OpenWindow,
    NewNote,
    NewLink,
    CaptureText(String),
    CaptureImage { path: String, mime_type: String },
    Quit,
}

/// A handle used by the status item (and anything else off the GPUI thread)
/// to ask the app to do something.
#[derive(Clone)]
pub struct CommandSender {
    tx: mpsc::UnboundedSender<AppCommand>,
}

impl CommandSender {
    pub fn send(&self, command: AppCommand) {
        let _ = self.tx.send(command);
    }
}

/// The application service: persistence + the AI worker.
pub struct WorktableService {
    tokio: Arc<Runtime>,
    runtime: Option<Arc<WorktableRuntime>>,
    /// In-memory fallback used when the local database is unavailable.
    memory: Arc<tokio::sync::Mutex<Vec<WorktableEntry>>>,
    /// In-memory config fallback when the database is unavailable.
    memory_config: Arc<tokio::sync::Mutex<HashMap<String, String>>>,
    persistent: bool,
    command_tx: mpsc::UnboundedSender<AppCommand>,
    command_rx: Option<std::sync::Mutex<Option<mpsc::UnboundedReceiver<AppCommand>>>>,
}

impl WorktableService {
    #[cfg(test)]
    pub fn new_for_test(db_path: &str) -> anyhow::Result<Self> {
        let tokio = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|error| anyhow!("failed to create Worktable Tokio runtime: {error}"))?,
        );
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        // Direct connect for hermetic tests (no env var, no bootstrap side-effects).
        let runtime = tokio.block_on(WorktableRuntime::connect(db_path))?;
        let entries = tokio
            .block_on(runtime.list_entries(500))
            .unwrap_or_default();
        // Start AI worker so `has_ai_worker` is true in tests (best-effort).
        let _ = tokio.block_on(runtime.start_ai_worker());
        let runtime = Some(Arc::new(runtime));
        let service = Self {
            tokio: tokio.clone(),
            runtime,
            memory: Arc::new(tokio::sync::Mutex::new(entries)),
            memory_config: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            persistent: true,
            command_tx,
            command_rx: Some(std::sync::Mutex::new(Some(command_rx))),
        };
        Ok(service)
    }

    pub fn new() -> anyhow::Result<Self> {
        let tokio = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|error| anyhow!("failed to create Worktable Tokio runtime: {error}"))?,
        );

        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let (runtime, persistent, seed) = bootstrap_runtime(&tokio);

        let service = Self {
            tokio,
            runtime,
            memory: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            memory_config: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            persistent,
            command_tx,
            command_rx: Some(std::sync::Mutex::new(Some(command_rx))),
        };

        // Seed the in-memory store with any entries we loaded from the local
        // database so the fallback path still shows the user's saved data.
        if let Some(entries) = seed {
            let memory = Arc::clone(&service.memory);
            service.tokio.spawn(async move {
                *memory.lock().await = entries;
            });
        }

        Ok(service)
    }

    pub fn command_sender(&self) -> CommandSender {
        CommandSender {
            tx: self.command_tx.clone(),
        }
    }

    pub fn take_command_receiver(&self) -> Option<mpsc::UnboundedReceiver<AppCommand>> {
        self.command_rx.as_ref()?.lock().ok()?.take()
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent
    }

    pub fn database_path(&self) -> String {
        self.runtime
            .as_ref()
            .map(|r| r.database_path().to_owned())
            .unwrap_or_else(|| {
                resolve_database_path().unwrap_or_else(|_| "/tmp/worktable.db".to_string())
            })
    }

    pub fn has_ai_worker(&self) -> bool {
        self.runtime.is_some()
    }

    /// Subscribe to runtime events (AI deltas etc.).
    pub fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<WorktableEvent>> {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.events().subscribe())
    }

    /// Fetch the list of entries.
    pub async fn list_entries(&self) -> ListResult {
        if let Some(runtime) = &self.runtime {
            runtime
                .list_entries(500)
                .await
                .map_err(|error| error.to_string())
        } else {
            Ok(self.memory.lock().await.clone())
        }
    }

    /// Insert an entry, persisting to the local database when available.
    ///
    /// When HelixDB is reachable the entry is also mirrored into the graph
    /// (label `Entry`, props `id/kind/content/title/source/created_at`) via
    /// `worktable-helix` best-effort — failures are logged but do not fail the
    /// insert, so SQLite remains the source-of-truth.
    pub async fn insert_entry(&self, entry: WorktableEntry) -> EmptyResult {
        if let Some(runtime) = &self.runtime {
            runtime
                .insert_entry(&entry)
                .await
                .map_err(|error| error.to_string())?;
        } else {
            // In-memory fallback (no SQLite) — still best-effort mirror to Helix.
            #[cfg(not(target_arch = "wasm32"))]
            {
                let entry_clone = entry.clone();
                self.tokio.spawn(async move {
                    let helix = worktable_helix::HelixClient::from_env();
                    if let Err(err) = helix.sync_entry(&entry_clone).await {
                        eprintln!("[worktable] Helix sync (memory mode) failed: {err}");
                    }
                });
            }
        }
        self.memory.lock().await.push(entry);
        Ok(())
    }

    /// Delete an entry by id.
    pub async fn delete_entry(&self, id: &str) -> EmptyResult {
        if let Some(runtime) = &self.runtime {
            runtime
                .delete_entry(id)
                .await
                .map_err(|error| error.to_string())?;
        }
        let mut memory = self.memory.lock().await;
        memory.retain(|entry| entry.id != id);
        Ok(())
    }

    /// Submit a prompt to the AI worker and return the run info.
    pub async fn submit_prompt(
        &self,
        request_id: &str,
        session_id: &str,
        content: &str,
    ) -> Result<Option<worktable_ai::AiRun>, String> {
        let request_id = request_id.to_owned();
        let session_id = session_id.to_owned();
        let content = content.to_owned();
        self.run_on_tokio(move |runtime| async move {
            runtime
                .submit_prompt(&request_id, &session_id, &content)
                .await
        })
        .await
    }

    /// Ask the worker for a fresh provider catalog + auth status snapshot.
    pub async fn list_providers(&self) -> EmptyResult {
        self.run_on_tokio(|runtime| async move { runtime.list_providers().await })
            .await
    }

    /// Store an API key for a provider (and activate it).
    pub async fn set_api_key(&self, provider_id: &str, api_key: &str) -> EmptyResult {
        let provider_id = provider_id.to_owned();
        let api_key = api_key.to_owned();
        self.run_on_tokio(move |runtime| async move {
            runtime.set_api_key(&provider_id, &api_key).await
        })
        .await
    }

    /// Select the active provider + model.
    pub async fn set_model(&self, provider_id: &str, model_id: &str) -> EmptyResult {
        let provider_id = provider_id.to_owned();
        let model_id = model_id.to_owned();
        self.run_on_tokio(
            move |runtime| async move { runtime.set_model(&provider_id, &model_id).await },
        )
        .await
    }

    /// Remove the stored credential for a provider.
    pub async fn logout_provider(&self, provider_id: &str) -> EmptyResult {
        let provider_id = provider_id.to_owned();
        self.run_on_tokio(move |runtime| async move { runtime.logout_provider(&provider_id).await })
            .await
    }

    /// Start an OAuth login flow for a provider.
    pub async fn login_oauth(&self, provider_id: &str) -> EmptyResult {
        let provider_id = provider_id.to_owned();
        self.run_on_tokio(move |runtime| async move { runtime.login_oauth(&provider_id).await })
            .await
    }

    /// Abort an in-progress OAuth login.
    pub async fn cancel_login(&self, provider_id: &str) -> EmptyResult {
        let provider_id = provider_id.to_owned();
        self.run_on_tokio(move |runtime| async move { runtime.cancel_login(&provider_id).await })
            .await
    }

    /// Answer a pending login prompt from the worker.
    pub async fn answer_auth_prompt(&self, prompt_id: &str, answer: &str) -> EmptyResult {
        let prompt_id = prompt_id.to_owned();
        let answer = answer.to_owned();
        self.run_on_tokio(move |runtime| async move {
            runtime.answer_auth_prompt(&prompt_id, &answer).await
        })
        .await
    }

    // ---- Generic wt_ai_config access (github_username, github_token, etc.) ----

    pub async fn get_config(&self, key: &str) -> Result<Option<String>, String> {
        if let Some(runtime) = self.runtime.clone() {
            let key = key.to_owned();
            let tokio = self.tokio.clone();
            tokio
                .spawn(async move { runtime.get_config(&key) })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        } else {
            Ok(self.memory_config.lock().await.get(key).cloned())
        }
    }

    pub async fn set_config(&self, key: &str, value: &str) -> Result<(), String> {
        if let Some(runtime) = self.runtime.clone() {
            let key = key.to_owned();
            let value = value.to_owned();
            let tokio = self.tokio.clone();
            tokio
                .spawn(async move { runtime.set_config(&key, &value) })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        } else {
            self.memory_config
                .lock()
                .await
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }
    }

    pub async fn delete_config(&self, key: &str) -> Result<(), String> {
        if let Some(runtime) = self.runtime.clone() {
            let key = key.to_owned();
            let tokio = self.tokio.clone();
            tokio
                .spawn(async move { runtime.delete_config(&key) })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
        } else {
            self.memory_config.lock().await.remove(key);
            Ok(())
        }
    }

    pub async fn get_github_username(&self) -> Result<Option<String>, String> {
        self.get_config("github_username").await
    }

    pub async fn set_github_username(&self, username: &str) -> Result<(), String> {
        let username = username.trim();
        if username.is_empty() {
            return self.delete_config("github_username").await;
        }
        self.set_config("github_username", username).await
    }

    /// Resolve optional GitHub token from (in priority order):
    /// 1) `GITHUB_TOKEN` env var
    /// 2) `wt_ai_config.github_token`
    /// 3) provider credential `github` (key field)
    pub async fn get_github_token(&self) -> Option<String> {
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            let t = token.trim().to_owned();
            if !t.is_empty() {
                return Some(t);
            }
        }
        if let Ok(Some(token)) = self.get_config("github_token").await {
            let t = token.trim().to_owned();
            if !t.is_empty() {
                return Some(t);
            }
        }
        if let Some(runtime) = self.runtime.clone() {
            let tokio = self.tokio.clone();
            if let Ok(Ok(Some(cred))) = tokio
                .spawn(async move { runtime.read_provider_credential("github") })
                .await
            {
                let t = cred.key.trim().to_owned();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        None
    }

    /// Fetch GitHub stars for `username`, using an optional token from credentials/env.
    pub async fn fetch_github_stars(
        &self,
        username: &str,
    ) -> Result<(u64, Vec<crate::github::GithubRepo>), String> {
        let token = self.get_github_token().await;
        let username = username.to_owned();
        let tokio = self.tokio.clone();
        tokio
            .spawn(async move { crate::github::fetch_github_stars(&username, token).await })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    pub async fn shutdown(&self) {
        if let Some(runtime) = &self.runtime {
            let _ = runtime.shutdown().await;
        }
    }

    /// Run a runtime operation on the service's Tokio runtime.
    ///
    /// The UI calls these database and worker operations from GPUI's executor,
    /// so the work is forwarded onto the service's dedicated Tokio runtime.
    async fn run_on_tokio<T, Fut, F>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(Arc<WorktableRuntime>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
        T: Send + 'static,
    {
        let Some(runtime) = self.runtime.clone() else {
            return Err("AI assistant is not configured".to_owned());
        };
        let tokio = self.tokio.clone();
        tokio
            .spawn(async move { f(runtime).await })
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }
}

/// Create (and start) the `WorktableRuntime` backed by a local database file.
fn bootstrap_runtime(
    tokio: &Arc<Runtime>,
) -> (
    Option<Arc<WorktableRuntime>>,
    bool,
    Option<Vec<WorktableEntry>>,
) {
    let database_path = match resolve_database_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Worktable: {error}");
            return (None, false, None);
        }
    };

    let runtime = match tokio.block_on(WorktableRuntime::connect(&database_path)) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Worktable: failed to open database at {database_path}: {error:#}");
            return (None, false, None);
        }
    };

    let entries = match tokio.block_on(runtime.list_entries(500)) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("Worktable: failed to load entries: {error:#}");
            Vec::new()
        }
    };

    start_ai_worker(tokio, &runtime);

    (Some(Arc::new(runtime)), true, Some(entries))
}

/// Resolve the local database file path: `WORKTABLE_DB_PATH` or
/// `~/.worktable/worktable.db`.
fn resolve_database_path() -> anyhow::Result<String> {
    if let Ok(path) = std::env::var("WORKTABLE_DB_PATH") {
        if !path.is_empty() {
            return Ok(path);
        }
    }

    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("neither WORKTABLE_DB_PATH nor HOME is set"))?;
    let dir = std::path::Path::new(&home).join(".worktable");
    std::fs::create_dir_all(&dir)
        .map_err(|error| anyhow::anyhow!("failed to create {}: {error}", dir.display()))?;
    Ok(dir.join("worktable.db").to_string_lossy().into_owned())
}

/// Start the embedded Pi agent runtime.
fn start_ai_worker(tokio: &Arc<Runtime>, runtime: &WorktableRuntime) {
    if let Err(error) = tokio.block_on(runtime.start_ai_worker()) {
        eprintln!("Worktable: failed to start the AI agent: {error:#}");
    }
}

pub fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub fn new_entry_id() -> String {
    Uuid::new_v4().to_string()
}

/// Convenience helper: spawn a one-shot future on the service runtime.
pub fn spawn_on_tokio<F, R>(tokio: &Arc<Runtime>, f: F) -> oneshot::Receiver<R>
where
    F: Future<Output = R> + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    tokio.spawn(async move {
        let _ = tx.send(f.await);
    });
    rx
}
