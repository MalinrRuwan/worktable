//! Pi Agent runtime — the AI agent embedded natively via `pi_agent_rust`.
//!
//! This replaces the previous AgentOS + JavaScript worker. The whole agent
//! ship (providers, streaming, model catalog, tool loop) is a Rust library
//! linked straight into the app, so there is no separate JS runtime, no
//! sidecar binary, and no `agent/` bundle to ship.
//!
//! `pi_agent_rust` runs on the `asupersync` async runtime, not Tokio. Each
//! request therefore runs to completion on its own OS thread (blocking) and
//! fans its `AgentEvent`s out to the existing `WorkerEvent` channel, which
//! `runtime.rs` drains just like before. Requests are serialized one at a time.

use std::{
    future::Future,
    sync::{Arc, Mutex},
};

use anyhow::{Context, anyhow};
use worktable_db::{ProviderCredential, SqliteStore};

use pi::model::AssistantMessageEvent;
use pi::sdk::{AbortHandle, AgentEvent, AgentSessionHandle, SessionOptions, create_agent_session};
use worktable_events::{ModelInfo, ProviderInfo, ProvidersSnapshot};

use crate::worker_protocol::{WorkerEvent, WorkerRequest};

#[cfg(not(target_arch = "wasm32"))]
use crate::helix_tool::HelixToolFactory;

const AUTH_FALLBACK_FILE: &str = ".worktable-pi-auth.json";

/// In-process Pi Agent runtime.
///
/// `send` spawns one blocking OS thread per request. The thread takes a
/// process-wide lock (so prompts/key changes run one at a time, matching the
/// old one-shot worker), performs the work, and emits `WorkerEvent`s into
/// `events_rx` via `events_tx`.
pub struct PiAgentRuntime {
    store: SqliteStore,
    events_tx: std::sync::mpsc::SyncSender<WorkerEvent>,
    events_rx: std::sync::Mutex<std::sync::mpsc::Receiver<WorkerEvent>>,
    request_lock: Arc<Mutex<()>>,
    active_abort: Arc<Mutex<Option<AbortHandle>>>,
}

impl PiAgentRuntime {
    /// Create the runtime. A `Ready` event is emitted once so the event pump
    /// reports the AI worker as ready.
    pub fn start(store: SqliteStore) -> Self {
        let (events_tx, events_rx) = std::sync::mpsc::sync_channel(256);
        let runtime = Self {
            store,
            events_tx,
            events_rx: std::sync::Mutex::new(events_rx),
            request_lock: Arc::new(Mutex::new(())),
            active_abort: Arc::new(Mutex::new(None)),
        };
        let _ = runtime.events_tx.send(WorkerEvent::Ready);
        runtime
    }

    pub fn try_recv(&self) -> Option<WorkerEvent> {
        self.events_rx.lock().unwrap().try_recv().ok()
    }

    /// Dispatch a request. Returns after the worker thread has been spawned.
    pub fn send(&self, request: WorkerRequest) -> anyhow::Result<()> {
        let store = self.store.clone();
        let events_tx = self.events_tx.clone();
        let request_lock = Arc::clone(&self.request_lock);
        let active_abort = Arc::clone(&self.active_abort);

        std::thread::Builder::new()
            .name("worktable-pi".to_owned())
            .spawn(move || {
                let _guard = request_lock.lock().unwrap();
                handle_request(request, &store, &events_tx, &active_abort);
            })
            .context("failed to spawn AI agent thread")?;

        Ok(())
    }

    pub fn shutdown(&self) {
        if let Some(abort) = self.active_abort.lock().unwrap().take() {
            abort.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

fn handle_request(
    request: WorkerRequest,
    store: &SqliteStore,
    events_tx: &std::sync::mpsc::SyncSender<WorkerEvent>,
    active_abort: &Arc<Mutex<Option<AbortHandle>>>,
) {
    match request {
        WorkerRequest::Prompt {
            request_id,
            session_id,
            content,
        } => run_prompt(
            store,
            events_tx,
            active_abort,
            &request_id,
            &session_id,
            &content,
        ),
        WorkerRequest::Cancel { request_id, .. } => {
            if let Some(abort) = active_abort.lock().unwrap().take() {
                abort.abort();
                let _ = events_tx.send(WorkerEvent::RunFailed {
                    request_id,
                    session_id: String::new(),
                    error: "aborted by user".to_owned(),
                });
            }
        }
        WorkerRequest::ListProviders => match build_snapshot(store) {
            Ok(snapshot) => {
                let _ = events_tx.send(WorkerEvent::ProvidersSnapshot { snapshot });
            }
            Err(error) => {
                let _ = events_tx.send(WorkerEvent::WorkerError {
                    error: format!("failed to list providers: {error}"),
                });
            }
        },
        WorkerRequest::SetApiKey {
            provider_id,
            api_key,
        } => {
            set_api_key(store, events_tx, &provider_id, &api_key);
        }
        WorkerRequest::SetModel {
            provider_id,
            model_id,
        } => {
            if store.set_config("active_provider", &provider_id).is_ok() {
                let _ = store.set_config("active_model", &model_id);
            }
            emit_config_changed(store, events_tx);
        }
        WorkerRequest::Logout { provider_id } => {
            let _ = store.delete_provider_credential(&provider_id);
            if store
                .get_config("active_provider")
                .ok()
                .flatten()
                .as_deref()
                == Some(provider_id.as_str())
            {
                let _ = store.set_config("active_provider", "");
                let _ = store.set_config("active_model", "");
            }
            emit_config_changed(store, events_tx);
        }
        WorkerRequest::LoginOAuth { provider_id } => {
            let _ = events_tx.send(WorkerEvent::LoginResult {
                provider_id,
                ok: false,
                error: Some(
                    "OAuth login is not available in the embedded agent; configure an API key instead"
                        .to_owned(),
                ),
            });
        }
        WorkerRequest::CancelLogin { .. } | WorkerRequest::AnswerAuthPrompt { .. } => {
            let _ = events_tx.send(WorkerEvent::WorkerError {
                error: "unsupported in embedded agent".to_owned(),
            });
        }
        WorkerRequest::Shutdown => {}
    }
}

// ---------------------------------------------------------------------------
// Prompt execution (via pi::sdk)
// ---------------------------------------------------------------------------

fn run_prompt(
    store: &SqliteStore,
    events_tx: &std::sync::mpsc::SyncSender<WorkerEvent>,
    active_abort: &Arc<Mutex<Option<AbortHandle>>>,
    request_id: &str,
    session_id: &str,
    content: &str,
) {
    let provider_id = match store.get_config("active_provider") {
        Ok(Some(provider)) if !provider.is_empty() => provider,
        _ => {
            let _ = events_tx.send(WorkerEvent::RunFailed {
                request_id: request_id.to_owned(),
                session_id: session_id.to_owned(),
                error: "no AI provider configured; open Settings to set one up".to_owned(),
            });
            return;
        }
    };
    let model_id = match store.get_config("active_model") {
        Ok(Some(model)) if !model.is_empty() => model,
        _ => {
            let _ = events_tx.send(WorkerEvent::RunFailed {
                request_id: request_id.to_owned(),
                session_id: session_id.to_owned(),
                error: format!("no model configured for provider {provider_id}"),
            });
            return;
        }
    };

    let api_key = store
        .read_provider_credential(&provider_id)
        .ok()
        .flatten()
        .map(|credential| credential.key);

    let working_directory = std::env::current_dir().ok();

    // --- Helix tooling -------------------------------------------------------
    // On native, expose `search_knowledge` so the LLM can retrieve Worktable
    // entries mirrored into HelixDB's graph. The tool is best-effort: when
    // Helix is not running it returns a fallback message and the agent can
    // still answer from its prompt context. On WASM we stay chat-only.
    #[cfg(not(target_arch = "wasm32"))]
    let (enabled_tools, tool_factory) = {
        let factory: std::sync::Arc<dyn pi::sdk::ToolFactory> =
            std::sync::Arc::new(HelixToolFactory);
        (Some(vec!["search_knowledge".to_string()]), Some(factory))
    };
    #[cfg(target_arch = "wasm32")]
    let (enabled_tools, tool_factory): (
        Option<Vec<String>>,
        Option<std::sync::Arc<dyn pi::sdk::ToolFactory>>,
    ) = (Some(Vec::new()), None);

    let options = SessionOptions {
        provider: Some(provider_id),
        model: Some(model_id),
        api_key,
        enabled_tools,
        tool_factory,
        working_directory: working_directory.clone(),
        include_cwd_in_prompt: false,
        ..Default::default()
    };

    let (abort_handle, abort_signal) = AgentSessionHandle::new_abort_handle();
    *active_abort.lock().unwrap() = Some(abort_handle);

    let tx = events_tx.clone();
    let request_id_owned = request_id.to_owned();
    let session_id_owned = session_id.to_owned();
    let on_event = move |event: AgentEvent| {
        if let Some(worker_event) = translate_event(&event, &request_id_owned, &session_id_owned) {
            let _ = tx.send(worker_event);
        }
    };

    let run = run_on_pi(async move {
        let mut handle = create_agent_session(options).await?;
        handle
            .prompt_with_abort(content, abort_signal, on_event)
            .await
    });

    *active_abort.lock().unwrap() = None;

    match run {
        Ok(_assistant) => {
            let _ = events_tx.send(WorkerEvent::RunCompleted {
                request_id: request_id.to_owned(),
                session_id: session_id.to_owned(),
            });
        }
        Err(error) => {
            let _ = events_tx.send(WorkerEvent::RunFailed {
                request_id: request_id.to_owned(),
                session_id: session_id.to_owned(),
                error: error.to_string(),
            });
        }
    }
}

/// Run a `pi` future to completion on a fresh asupersync current-thread runtime.
fn run_on_pi<T>(future: impl Future<Output = Result<T, pi::Error>>) -> anyhow::Result<T> {
    let reactor = asupersync::runtime::reactor::create_reactor()
        .map_err(|error| anyhow!("failed to create pi reactor: {error}"))?;
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .map_err(|error| anyhow!("failed to build pi runtime: {error}"))?;
    runtime.block_on(future).map_err(|error| anyhow!("{error}"))
}

fn translate_event(event: &AgentEvent, request_id: &str, session_id: &str) -> Option<WorkerEvent> {
    match event {
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } => {
                Some(WorkerEvent::AgentMessageDelta {
                    request_id: request_id.to_owned(),
                    session_id: session_id.to_owned(),
                    delta: delta.clone(),
                })
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                Some(WorkerEvent::AgentThoughtDelta {
                    request_id: request_id.to_owned(),
                    session_id: session_id.to_owned(),
                    delta: delta.clone(),
                })
            }
            _ => None,
        },
        AgentEvent::ToolExecutionStart { tool_name, .. } => Some(WorkerEvent::ToolStarted {
            request_id: request_id.to_owned(),
            session_id: session_id.to_owned(),
            tool_call_id: String::new(),
            name: tool_name.clone(),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Provider / model configuration
// ---------------------------------------------------------------------------

fn set_api_key(
    store: &SqliteStore,
    events_tx: &std::sync::mpsc::SyncSender<WorkerEvent>,
    provider_id: &str,
    api_key: &str,
) {
    let credential = ProviderCredential {
        kind: "api_key".to_owned(),
        key: api_key.to_owned(),
    };
    if let Err(error) = store.write_provider_credential(provider_id, &credential) {
        let _ = events_tx.send(WorkerEvent::WorkerError {
            error: format!("failed to store API key: {error}"),
        });
        return;
    }

    // Activate the provider if nothing is selected yet.
    if store.get_config("active_provider").ok().flatten().is_none() {
        let _ = store.set_config("active_provider", provider_id);
    }
    emit_config_changed(store, events_tx);
}

fn emit_config_changed(store: &SqliteStore, events_tx: &std::sync::mpsc::SyncSender<WorkerEvent>) {
    let active_provider = store.get_config("active_provider").ok().flatten();
    let active_model = store.get_config("active_model").ok().flatten();
    let _ = events_tx.send(WorkerEvent::ConfigChanged {
        active_provider: active_provider.filter(|value| !value.is_empty()),
        active_model: active_model.filter(|value| !value.is_empty()),
    });
    let _ = events_tx.send(WorkerEvent::ProvidersSnapshot {
        snapshot: build_snapshot(store).unwrap_or_else(|_| ProvidersSnapshot {
            providers: Vec::new(),
            active_provider: None,
            active_model: None,
        }),
    });
}

/// Build the provider/model catalog for the Settings panel.
///
/// The provider list comes from pi's canonical provider metadata; the model
/// list for each provider comes from pi's built-in model registry. Credential
/// status comes from Worktable's own database (so the UI reflects keys stored
/// in `~/.worktable/worktable.db`, independent of `~/.pi/`).
fn build_snapshot(store: &SqliteStore) -> anyhow::Result<ProvidersSnapshot> {
    let auth = load_empty_auth()?;
    let registry = pi::models::ModelRegistry::load(&auth, None);
    let credentials = store.list_provider_credentials()?;
    let active_provider = store.get_config("active_provider")?;
    let active_model = store.get_config("active_model")?;

    let mut providers = Vec::new();
    for meta in pi::provider_metadata::PROVIDER_METADATA {
        let Some(name) = meta.display_name else {
            continue;
        };
        let supports_api_key = !meta.auth_env_keys.is_empty();
        let keyless = pi::provider_metadata::provider_is_keyless_local(meta.canonical_id);
        // Only expose providers the user can configure with an API key (or
        // keyless local servers).
        if !supports_api_key && !keyless {
            continue;
        }

        let credential = credentials.get(meta.canonical_id);
        let models = {
            let canonical = meta.canonical_id;
            let mut list: Vec<ModelInfo> = registry
                .models()
                .iter()
                .filter(|entry| {
                    pi::provider_metadata::canonical_provider_id(&entry.model.provider)
                        .is_some_and(|c| c == canonical)
                })
                .map(|entry| ModelInfo {
                    id: entry.model.id.clone(),
                    name: if entry.model.name.is_empty() {
                        entry.model.id.clone()
                    } else {
                        entry.model.name.clone()
                    },
                })
                .collect();
            list.sort_by(|a, b| {
                a.name
                    .to_ascii_lowercase()
                    .cmp(&b.name.to_ascii_lowercase())
            });
            list.dedup_by(|a, b| a.id.eq_ignore_ascii_case(&b.id));
            list
        };

        providers.push(ProviderInfo {
            id: meta.canonical_id.to_owned(),
            name: name.to_owned(),
            supports_api_key,
            supports_oauth: false,
            api_key_set: credential.is_some(),
            oauth_set: false,
            models,
        });
    }

    Ok(ProvidersSnapshot {
        providers,
        active_provider: active_provider.filter(|value| !value.is_empty()),
        active_model: active_model.filter(|value| !value.is_empty()),
    })
}

/// pi's `ModelRegistry` needs an `AuthStorage`. We hand it an empty one backed
/// by a throwaway file in the temp dir; the model catalog itself never needs
/// credentials.
fn load_empty_auth() -> anyhow::Result<pi::auth::AuthStorage> {
    let auth_path =
        std::env::temp_dir().join(format!("{AUTH_FALLBACK_FILE}.{}", std::process::id()));
    std::fs::write(&auth_path, "{}")
        .with_context(|| format!("failed to write {}", auth_path.display()))?;
    pi::auth::AuthStorage::load(auth_path).map_err(|error| anyhow!("{error}"))
}
