//! Bridge between the GPUI main thread and the async `WorktableRuntime`.
//!
//! All database work runs on a dedicated Tokio runtime so the UI never blocks.
//! When Turso is not configured the service transparently falls back to an
//! in-memory store so the app stays fully usable.

use std::{
    collections::BTreeMap,
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
    NewNote,
    NewLink,
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
    /// In-memory fallback used when Turso is not configured.
    memory: Arc<tokio::sync::Mutex<Vec<WorktableEntry>>>,
    persistent: bool,
    command_tx: mpsc::UnboundedSender<AppCommand>,
    command_rx: Option<mpsc::UnboundedReceiver<AppCommand>>,
}

impl WorktableService {
    pub fn new() -> anyhow::Result<Self> {
        let tokio = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|error| anyhow!("failed to create Worktable Tokio runtime: {error}"))?,
        );

        let (command_tx, command_rx) = mpsc::unbounded_channel();

        let (runtime, persistent, seed) = bootstrap_runtime(&tokio);

        let mut service = Self {
            tokio,
            runtime,
            memory: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            persistent,
            command_tx,
            command_rx: Some(command_rx),
        };

        // Seed the in-memory store with any entries we loaded from Turso so the
        // fallback path still shows the user's saved data.
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

    pub fn take_command_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<AppCommand>> {
        self.command_rx.take()
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent
    }

    pub fn has_ai_worker(&self) -> bool {
        self.runtime.is_some()
    }

    /// Subscribe to runtime events (AI deltas etc.).
    pub fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<WorktableEvent>> {
        self.runtime.as_ref().map(|runtime| runtime.events().subscribe())
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

    /// Insert an entry, persisting to Turso when available.
    pub async fn insert_entry(&self, entry: WorktableEntry) -> EmptyResult {
        if let Some(runtime) = &self.runtime {
            runtime
                .insert_entry(&entry)
                .await
                .map_err(|error| error.to_string())?;
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
        let Some(runtime) = &self.runtime else {
            return Err("AI assistant is not configured".to_owned());
        };
        runtime
            .submit_prompt(request_id, session_id, content)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn shutdown(&self) {
        if let Some(runtime) = &self.runtime {
            let _ = runtime.shutdown().await;
        }
    }
}

/// Create (and start) the `WorktableRuntime` when Turso is configured.
fn bootstrap_runtime(
    tokio: &Arc<Runtime>,
) -> (Option<Arc<WorktableRuntime>>, bool, Option<Vec<WorktableEntry>>) {
    let database_url = std::env::var("TURSO_DATABASE_URL").ok();
    let auth_token = std::env::var("TURSO_AUTH_TOKEN").ok();

    let (Some(database_url), Some(auth_token)) = (database_url, auth_token) else {
        eprintln!("Worktable: Turso is not configured; running with in-memory storage");
        return (None, false, None);
    };

    let runtime = match tokio.block_on(WorktableRuntime::connect(&database_url, &auth_token)) {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Worktable: failed to connect to Turso: {error:#}");
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

/// Start the AgentOS Pi worker when it has been built.
fn start_ai_worker(tokio: &Arc<Runtime>, runtime: &WorktableRuntime) {
    const DEFAULT_WORKER_PATH: &str = "agent/dist/worker.js";

    let worker_path =
        std::env::var("WORKTABLE_PI_WORKER").unwrap_or_else(|_| DEFAULT_WORKER_PATH.to_owned());
    if !std::path::Path::new(&worker_path).exists() {
        eprintln!(
            "Worktable: AI worker is not built; run `cd agent && npm run build` or set WORKTABLE_PI_WORKER (currently `{worker_path}`)"
        );
        return;
    }

    let worker_command =
        std::env::var("WORKTABLE_PI_WORKER_COMMAND").unwrap_or_else(|_| "node".to_owned());
    let environment = worker_environment();
    if let Err(error) = tokio.block_on(runtime.start_ai_worker(
        &worker_command,
        vec![worker_path],
        environment,
    )) {
        eprintln!("Worktable: failed to start the AgentOS Pi worker: {error:#}");
    }
}

fn worker_environment() -> BTreeMap<String, String> {
    const FORWARDED_VARS: &[&str] = &[
        "PI_PROVIDER",
        "PI_MODEL",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GOOGLE_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
        "MISTRAL_API_KEY",
        "XAI_API_KEY",
        "GROQ_API_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_REGION",
    ];

    FORWARDED_VARS
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .map(|value| ((*name).to_owned(), value))
        })
        .collect()
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
