//! Bridge between the GPUI main thread and the async `WorktableRuntime`.
//!
//! All database work runs on a dedicated Tokio runtime so the UI never blocks.
//! When the local SQLite database cannot be opened the service transparently
//! falls back to an in-memory store so the app stays fully usable.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::anyhow;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use uuid::Uuid;
use worktable_ai::{WorktableEntry, WorktableRuntime};
use worktable_events::WorktableEvent;

/// Config key holding the JSON array of GitHub `full_name`s that have ever
/// been imported as entries, so re-imports stay idempotent across deletes.
const IMPORTED_STARS_CONFIG_KEY: &str = "github_imported_repos";

pub type ListResult = Result<Vec<WorktableEntry>, String>;
pub type EmptyResult = Result<(), String>;

/// Commands the macOS menu-bar status item can send into the app.
#[derive(Debug, Clone)]
pub enum AppCommand {
    ToggleWindow,
    OpenWindow,
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
    memory_chats: tokio::sync::Mutex<HashMap<String, worktable_db::StoredChat>>,
    /// Internal media library: added images are hard-linked here (falling back
    /// to a copy) so entries keep working when the original moves away.
    media_dir: PathBuf,
    command_tx: mpsc::UnboundedSender<AppCommand>,
    command_rx: Option<std::sync::Mutex<Option<mpsc::UnboundedReceiver<AppCommand>>>>,
}

impl WorktableService {
    #[cfg(any(test, feature = "visual-tests"))]
    // The main binary also compiles under `visual-tests`; only the visual
    // runner and the test harness call this hermetic constructor.
    #[allow(dead_code)]
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
        // Start AI worker so the runtime is prompt-ready in tests (best-effort).
        start_ai_worker(&tokio, &runtime);
        let runtime = Some(Arc::new(runtime));
        let media_dir = test_media_dir(db_path);
        let service = Self {
            tokio: tokio.clone(),
            runtime,
            memory: Arc::new(tokio::sync::Mutex::new(entries)),
            memory_config: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            memory_chats: tokio::sync::Mutex::new(HashMap::new()),
            media_dir,
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

        let (runtime, seed) = bootstrap_runtime(&tokio);

        let service = Self {
            tokio,
            runtime,
            memory: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            memory_config: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            memory_chats: tokio::sync::Mutex::new(HashMap::new()),
            media_dir: resolve_media_dir(),
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

    /// The internal media library directory (`~/.worktable/media`).
    pub fn media_dir(&self) -> &Path {
        &self.media_dir
    }

    /// Hard-link an image into the media library and return its stored path.
    ///
    /// Re-adding the same file reuses the existing entry; a cross-device or
    /// permission-limited link falls back to a copy. Hard links cost no extra
    /// disk space and survive the original moving away.
    pub fn import_image(&self, source: &Path) -> anyhow::Result<PathBuf> {
        let media_dir = self.media_dir();
        std::fs::create_dir_all(media_dir).map_err(|error| {
            anyhow!(
                "failed to create media directory {}: {error}",
                media_dir.display()
            )
        })?;

        let canonical = source
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf());
        if canonical.starts_with(media_dir) {
            return Ok(canonical);
        }

        let file_name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("image path has no file name: {}", source.display()))?;
        let (stem, extension) = match file_name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => {
                (stem.to_owned(), Some(extension.to_owned()))
            }
            _ => (file_name.clone(), None),
        };
        let source_size = source.metadata().ok().map(|meta| meta.len());

        let mut target = media_dir.join(&file_name);
        let mut suffix = 1;
        while target.exists() {
            // Same name and size: assume the same photo and reuse it instead
            // of accumulating duplicates.
            let same_size = target
                .metadata()
                .ok()
                .map(|meta| meta.len())
                .zip(source_size)
                .is_some_and(|(a, b)| a == b);
            if same_size {
                return Ok(target);
            }
            suffix += 1;
            let name = match &extension {
                Some(extension) => format!("{stem}-{suffix}.{extension}"),
                None => format!("{stem}-{suffix}"),
            };
            target = media_dir.join(name);
        }

        if std::fs::hard_link(source, &target).is_err() {
            std::fs::copy(source, &target)?;
        }
        Ok(target)
    }

    pub fn database_path(&self) -> String {
        self.runtime
            .as_ref()
            .map(|r| r.database_path().to_owned())
            .unwrap_or_else(|| {
                resolve_database_path().unwrap_or_else(|_| {
                    std::env::temp_dir()
                        .join("worktable.db")
                        .to_string_lossy()
                        .into_owned()
                })
            })
    }

    /// True when the persistent runtime (and therefore the AI worker host)
    /// was connected at startup. The worker itself is started best-effort;
    /// prompt submission surfaces its own errors if it is unavailable.
    pub fn has_ai_runtime(&self) -> bool {
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

    pub async fn list_chats(&self) -> Result<Vec<worktable_db::ChatSummary>, String> {
        if self.runtime.is_some() {
            self.run_on_tokio(|runtime| async move { runtime.list_chats() })
                .await
        } else {
            let mut chats: Vec<_> = self
                .memory_chats
                .lock()
                .await
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
    }

    pub async fn load_chat(&self, id: &str) -> Result<Option<worktable_db::StoredChat>, String> {
        if self.runtime.is_some() {
            let id = id.to_owned();
            self.run_on_tokio(move |runtime| async move { runtime.load_chat(&id) })
                .await
        } else {
            Ok(self.memory_chats.lock().await.get(id).cloned())
        }
    }

    pub async fn save_chat(&self, chat: worktable_db::StoredChat) -> EmptyResult {
        if self.runtime.is_some() {
            self.run_on_tokio(move |runtime| async move { runtime.save_chat(&chat) })
                .await
        } else {
            let mut chats = self.memory_chats.lock().await;
            if chats
                .get(&chat.summary.id)
                .is_none_or(|existing| chat.summary.revision >= existing.summary.revision)
            {
                chats.insert(chat.summary.id.clone(), chat);
            }
            Ok(())
        }
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

    /// Extract AI topics for knowledge-graph entries with the active provider.
    /// Returns `(entry_id, topics)` pairs; empty when no provider is set up.
    pub async fn enrich_topics(
        &self,
        entries: Vec<worktable_ai::WorktableEntry>,
    ) -> Result<Vec<(String, Vec<String>)>, String> {
        self.run_on_tokio(move |runtime| async move { runtime.enrich_topics(entries).await })
            .await
            .map_err(|error| error.to_string())
    }

    /// Replace an entry's content (the detail editor's Save).
    pub async fn update_entry(&self, entry: worktable_ai::WorktableEntry) -> EmptyResult {
        if let Some(runtime) = self.runtime.clone() {
            runtime
                .update_entry(&entry)
                .await
                .map_err(|error| error.to_string())
        } else {
            // In-memory fallback: replace by id.
            let mut memory = self.memory.lock().await;
            if let Some(existing) = memory.iter_mut().find(|item| item.id == entry.id) {
                *existing = entry;
            }
            Ok(())
        }
    }

    /// Abort the in-flight assistant run for a session.
    pub async fn cancel_prompt(&self, session_id: &str) -> EmptyResult {
        let session_id = session_id.to_owned();
        self.run_on_tokio(
            move |runtime| async move { runtime.cancel_prompt("", &session_id).await },
        )
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

    /// Import `username`'s starred repositories as entries. One link entry
    /// per star: title = `owner/repo`, content = the repo description (or its
    /// URL when there is none), `created_at` = GitHub's `starred_at`
    /// timestamp. Stars already imported (matched by title + source) are
    /// skipped. Returns `(imported, skipped)`.
    pub async fn import_starred_repos(
        &self,
        username: &str,
        token: Option<String>,
    ) -> Result<(usize, usize), String> {
        self.import_starred_repos_with_base(username, token, "https://api.github.com")
            .await
    }

    /// Test seam for [`import_starred_repos`]: the API root is injectable.
    ///
    /// The HTTP fetch runs on the service's dedicated Tokio runtime — reqwest
    /// needs a Tokio reactor, and this future is polled on GPUI's executor
    /// (which has none). Awaiting it inline crashed the app with "there is no
    /// reactor running".
    pub async fn import_starred_repos_with_base(
        &self,
        username: &str,
        token: Option<String>,
        api_base: &str,
    ) -> Result<(usize, usize), String> {
        let username = username.to_owned();
        let api_base = api_base.to_owned();
        let tokio = self.tokio.clone();
        let starred = tokio
            .spawn(async move {
                crate::github::fetch_starred_repos_with_base(&username, token, &api_base).await
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;

        // Import is idempotent by a persistent ledger, not just by what is
        // currently visible: deleting an imported entry must not let the next
        // import resurrect it as a duplicate. Entries imported before the
        // ledger existed are folded in from the store itself.
        let mut imported_names: std::collections::HashSet<String> = self
            .get_config(IMPORTED_STARS_CONFIG_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .collect();
        imported_names.extend(
            self.list_entries()
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| entry.source == "github-star")
                .filter_map(|entry| entry.title),
        );

        let mut imported = 0usize;
        let mut skipped = 0usize;
        for repo in starred {
            if imported_names.contains(&repo.full_name) {
                skipped += 1;
                continue;
            }
            let content = repo
                .description
                .clone()
                .filter(|d| !d.trim().is_empty())
                .unwrap_or_else(|| repo.html_url.clone());
            let entry = WorktableEntry {
                id: new_entry_id(),
                content,
                title: Some(repo.full_name.clone()),
                source: "github-star".to_owned(),
                created_at: if repo.starred_at_ms > 0 {
                    repo.starred_at_ms
                } else {
                    unix_time_ms()
                },
            };
            match self.insert_entry(entry).await {
                Ok(()) => {
                    imported += 1;
                    imported_names.insert(repo.full_name);
                }
                Err(error) => {
                    return Err(format!("Imported {imported} before failing: {error}"));
                }
            }
        }

        let mut names: Vec<String> = imported_names.into_iter().collect();
        names.sort();
        if let Ok(raw) = serde_json::to_string(&names) {
            let _ = self.set_config(IMPORTED_STARS_CONFIG_KEY, &raw).await;
        }
        Ok((imported, skipped))
    }

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
/// Returns the runtime (when the database could be opened) and the entries
/// loaded for the in-memory fallback store.
fn bootstrap_runtime(
    tokio: &Arc<Runtime>,
) -> (Option<Arc<WorktableRuntime>>, Option<Vec<WorktableEntry>>) {
    let database_path = match resolve_database_path() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Worktable: {error}");
            return (None, None);
        }
    };

    let runtime = match tokio.block_on(WorktableRuntime::connect(&database_path)) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Worktable: failed to open database at {database_path}: {error:#}");
            return (None, None);
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

    (Some(Arc::new(runtime)), Some(entries))
}

/// Internal media library for the real app: `WORKTABLE_MEDIA_DIR` or
/// `~/.worktable/media`.
fn resolve_media_dir() -> PathBuf {
    if let Ok(path) = std::env::var("WORKTABLE_MEDIA_DIR")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    Path::new(&home).join(".worktable").join("media")
}

/// Hermetic media library for tests: next to the temp database, so no test
/// ever touches the user's real `~/.worktable`.
#[cfg(any(test, feature = "visual-tests"))]
fn test_media_dir(db_path: &str) -> PathBuf {
    Path::new(db_path)
        .parent()
        .map(|parent| parent.join("media"))
        .unwrap_or_else(|| std::env::temp_dir().join("worktable-media"))
}

/// Resolve the local database file path: `WORKTABLE_DB_PATH` or
/// `~/.worktable/worktable.db`.
fn resolve_database_path() -> anyhow::Result<String> {
    if let Ok(path) = std::env::var("WORKTABLE_DB_PATH")
        && !path.is_empty()
    {
        return Ok(path);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Block a future to completion WITHOUT a Tokio reactor — mirroring how
    /// the view calls service methods from GPUI's executor. Awaiting reqwest
    /// inline under this executor is exactly what crashed the app.
    fn block_on_no_reactor<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn noop_raw_waker() -> RawWaker {
            RawWaker::new(std::ptr::null(), &NOOP_VTABLE)
        }
        static NOOP_VTABLE: RawWakerVTable =
            RawWakerVTable::new(|_| noop_raw_waker(), |_| {}, |_| {}, |_| {});
        let waker = unsafe { Waker::from_raw(noop_raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    /// A one-shot HTTP/1.1 server speaking enough of the GitHub starred-repos
    /// API (star+json media type) for the import path to run against.
    struct MockGithub {
        addr: std::net::SocketAddr,
        shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl MockGithub {
        fn start(body: &'static str) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = shutdown.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    let Ok(mut stream) = stream else { continue };
                    let mut buf = [0u8; 4096];
                    let _ = std::io::Read::read(&mut stream, &mut buf);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = std::io::Write::write_all(&mut stream, response.as_bytes());
                }
            });
            Self { addr, shutdown }
        }

        fn base_url(&self) -> String {
            format!("http://{}", self.addr)
        }
    }

    impl Drop for MockGithub {
        fn drop(&mut self) {
            self.shutdown
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    const STARRED_BODY: &str = r#"[
        {
            "starred_at": "2026-08-01T10:00:00Z",
            "repo": {
                "name": "gpui",
                "full_name": "zed-industries/gpui",
                "description": "Fast, productive tooling for building native apps",
                "stargazers_count": 12000,
                "html_url": "https://github.com/zed-industries/gpui"
            }
        },
        {
            "starred_at": "2026-07-15T08:30:00Z",
            "repo": {
                "name": "no-desc-repo",
                "full_name": "someone/no-desc-repo",
                "stargazers_count": 5,
                "html_url": "https://github.com/someone/no-desc-repo"
            }
        }
    ]"#;

    fn service_for_test() -> (Arc<WorktableService>, String) {
        let dir = std::env::temp_dir().join(format!("wt-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("worktable.db").to_string_lossy().into_owned();
        let service = Arc::new(WorktableService::new_for_test(&db).unwrap());
        (service, db)
    }

    /// The end-to-end import test — run under a reactor-free executor, exactly
    /// like the GPUI call site that crashed the app before the fix.
    #[test]
    fn import_starred_repos_creates_entries_with_starred_at_and_description() {
        let (service, _db) = service_for_test();
        let mock = MockGithub::start(STARRED_BODY);

        let result = block_on_no_reactor(service.import_starred_repos_with_base(
            "octocat",
            None,
            &mock.base_url(),
        ));
        let (imported, skipped) = result.expect("import succeeds without a Tokio reactor");
        assert_eq!(imported, 2, "both stars import");
        assert_eq!(skipped, 0);

        let entries = block_on_no_reactor(service.list_entries()).expect("list");
        assert_eq!(entries.len(), 2);

        let by_title = |t: &str| {
            entries
                .iter()
                .find(|e| e.title.as_deref() == Some(t))
                .unwrap()
        };
        let gpui_entry = by_title("zed-industries/gpui");
        assert_eq!(gpui_entry.source, "github-star");
        assert_eq!(
            gpui_entry.content, "Fast, productive tooling for building native apps",
            "description becomes the entry content"
        );
        assert_eq!(
            gpui_entry.created_at, 1_785_578_400_000,
            "created_at is the starred_at timestamp"
        );

        // No description → content falls back to the repo URL.
        let bare = by_title("someone/no-desc-repo");
        assert_eq!(bare.content, "https://github.com/someone/no-desc-repo");
        assert_eq!(bare.created_at, 1_784_104_200_000);
    }

    /// Re-running the import skips everything already present.
    #[test]
    fn import_starred_repos_is_idempotent() {
        let (service, _db) = service_for_test();
        let mock = MockGithub::start(STARRED_BODY);

        let first = block_on_no_reactor(service.import_starred_repos_with_base(
            "octocat",
            None,
            &mock.base_url(),
        ))
        .expect("first import");
        assert_eq!(first, (2, 0));

        let second = block_on_no_reactor(service.import_starred_repos_with_base(
            "octocat",
            None,
            &mock.base_url(),
        ))
        .expect("second import");
        assert_eq!(second.0, 0, "nothing re-imported");
        assert_eq!(second.1, 2, "both recognized as already present");

        let entries = block_on_no_reactor(service.list_entries()).expect("list");
        assert_eq!(entries.len(), 2, "no duplicates created");

        // Deleting an imported entry must not resurrect it later: the ledger
        // remembers every repo that was imported, not just what is visible.
        for entry in entries {
            block_on_no_reactor(service.delete_entry(&entry.id)).expect("delete");
        }
        assert!(
            block_on_no_reactor(service.list_entries())
                .expect("list")
                .is_empty(),
            "entries are deleted"
        );
        let third = block_on_no_reactor(service.import_starred_repos_with_base(
            "octocat",
            None,
            &mock.base_url(),
        ))
        .expect("third import after delete");
        assert_eq!(
            third,
            (0, 2),
            "a deleted star stays imported (idempotent across deletes)"
        );
    }

    /// HTTP failures surface as errors, never panics.
    #[test]
    fn import_starred_repos_surfaces_http_errors() {
        let (service, _db) = service_for_test();
        // Nothing is listening on this port.
        let result = block_on_no_reactor(service.import_starred_repos_with_base(
            "octocat",
            None,
            "http://127.0.0.1:9",
        ));
        let error = result.expect_err("connection refused should be an error");
        assert!(!error.is_empty());
    }
}
