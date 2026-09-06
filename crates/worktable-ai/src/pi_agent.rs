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

/// Error string used when a run ends because the user cancelled it. The UI
/// matches on this to avoid surfacing cancellations as failures.
pub const ABORTED_BY_USER: &str = "aborted by user";

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
    ///
    /// `Cancel` is handled synchronously on the caller's thread: queueing it
    /// behind `request_lock` would block until the very run it is meant to
    /// abort has finished, making cancellation a no-op.
    pub fn send(&self, request: WorkerRequest) -> anyhow::Result<()> {
        if let WorkerRequest::Cancel { request_id, .. } = request {
            if let Some(abort) = self.active_abort.lock().unwrap().take() {
                abort.abort();
                let _ = self.events_tx.send(WorkerEvent::RunFailed {
                    request_id,
                    session_id: String::new(),
                    error: ABORTED_BY_USER.to_owned(),
                });
            }
            return Ok(());
        }

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
        WorkerRequest::Cancel { .. } => {
            // Handled synchronously in `send` — a Cancel that lands here was
            // queued before the fast path existed; treat as a no-op.
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
    let active_provider = store.get_config("active_provider").ok().flatten();
    match active_provider.as_deref() {
        None | Some("") => {
            let _ = store.set_config("active_provider", provider_id);
        }
        Some(current) if current == provider_id => {}
        Some(_) => {}
    }

    // A provider without a model can never serve a prompt ("no model
    // configured"). Selecting a model is a separate, easy-to-miss UI step —
    // so default to the provider's first model whenever none is set (or the
    // stored one belongs to a different provider). The model picker can still
    // refine the choice afterwards.
    let active_model = store.get_config("active_model").ok().flatten();
    let model_needed = match active_model.as_deref() {
        None | Some("") => true,
        Some(model) => {
            !model_known_for_provider(provider_id, model)
        }
    };
    if model_needed
        && let Some(first) = first_model_for_provider(provider_id)
    {
        let _ = store.set_config("active_model", &first);
    }

    emit_config_changed(store, events_tx);
}

/// Whether `model_id` exists in pi's registry under `provider_id`.
fn model_known_for_provider(provider_id: &str, model_id: &str) -> bool {
    let Ok(auth) = load_empty_auth() else {
        return false;
    };
    let registry = pi::models::ModelRegistry::load(&auth, None);
    registry.models().iter().any(|entry| {
        entry.model.id == model_id
            && pi::provider_metadata::canonical_provider_id(&entry.model.provider)
                .is_some_and(|c| c == provider_id)
    })
}

/// The registry's first model id for `provider_id` (registry order), if any.
fn first_model_for_provider(provider_id: &str) -> Option<String> {
    let auth = load_empty_auth().ok()?;
    let registry = pi::models::ModelRegistry::load(&auth, None);
    registry
        .models()
        .iter()
        .find(|entry| {
            pi::provider_metadata::canonical_provider_id(&entry.model.provider)
                .is_some_and(|c| c == provider_id)
        })
        .map(|entry| entry.model.id.clone())
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn temp_store() -> SqliteStore {
        let store = SqliteStore::connect(":memory:").expect("connect in-memory");
        store.migrate().expect("migrate");
        store
    }

    /// Poll the worker event channel until `pred` matches or `timeout` elapses.
    /// Non-matching events are discarded (they are orthogonal traffic).
    fn recv_until(
        runtime: &PiAgentRuntime,
        timeout: Duration,
        pred: impl Fn(&WorkerEvent) -> bool,
    ) -> Option<WorkerEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(event) = runtime.try_recv() {
                if pred(&event) {
                    return Some(event);
                }
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        None
    }

    #[test]
    fn ready_is_emitted_on_start() {
        let runtime = PiAgentRuntime::start(temp_store());
        let event = recv_until(&runtime, Duration::from_secs(2), |e| {
            matches!(e, WorkerEvent::Ready)
        });
        assert!(event.is_some(), "worker should announce Ready on start");
    }

    #[test]
    fn list_providers_returns_catalog() {
        let runtime = PiAgentRuntime::start(temp_store());
        // Drain the Ready event first.
        let _ = runtime.try_recv();

        runtime.send(WorkerRequest::ListProviders).unwrap();
        let event = recv_until(&runtime, Duration::from_secs(10), |e| {
            matches!(e, WorkerEvent::ProvidersSnapshot { .. })
        });
        let Some(WorkerEvent::ProvidersSnapshot { snapshot }) = event else {
            panic!("expected a ProvidersSnapshot, got none");
        };
        assert!(
            !snapshot.providers.is_empty(),
            "provider catalog should not be empty"
        );
        for provider in &snapshot.providers {
            assert!(!provider.id.is_empty());
            assert!(!provider.name.is_empty());
        }
        assert!(
            snapshot.providers.iter().any(|p| p.id == "openai"),
            "openai should be in the catalog"
        );
        // Nothing is configured yet.
        assert!(snapshot.active_provider.is_none());
        assert!(snapshot.active_model.is_none());
    }

    #[test]
    fn set_api_key_activates_provider_and_reports_snapshot() {
        let store = temp_store();
        let runtime = PiAgentRuntime::start(store.clone());
        let _ = runtime.try_recv();

        runtime
            .send(WorkerRequest::SetApiKey {
                provider_id: "openai".to_owned(),
                api_key: "sk-test-key".to_owned(),
            })
            .unwrap();

        let event = recv_until(&runtime, Duration::from_secs(10), |e| {
            matches!(e, WorkerEvent::ProvidersSnapshot { .. })
        });
        let Some(WorkerEvent::ProvidersSnapshot { snapshot }) = event else {
            panic!("expected a ProvidersSnapshot after SetApiKey");
        };
        // Setting the first key activates the provider.
        assert_eq!(snapshot.active_provider.as_deref(), Some("openai"));
        let openai = snapshot
            .providers
            .iter()
            .find(|p| p.id == "openai")
            .expect("openai in snapshot");
        assert!(openai.api_key_set, "snapshot should report the stored key");

        // The credential is in the store, not just the snapshot.
        let credential = store
            .read_provider_credential("openai")
            .unwrap()
            .expect("credential stored");
        assert_eq!(credential.key, "sk-test-key");
    }

    #[test]
    fn set_model_persists_selection() {
        let store = temp_store();
        let runtime = PiAgentRuntime::start(store.clone());
        let _ = runtime.try_recv();

        runtime
            .send(WorkerRequest::SetModel {
                provider_id: "openai".to_owned(),
                model_id: "gpt-4o".to_owned(),
            })
            .unwrap();

        let event = recv_until(&runtime, Duration::from_secs(10), |e| {
            matches!(e, WorkerEvent::ConfigChanged { .. })
        });
        let Some(WorkerEvent::ConfigChanged {
            active_provider,
            active_model,
        }) = event
        else {
            panic!("expected ConfigChanged after SetModel");
        };
        assert_eq!(active_provider.as_deref(), Some("openai"));
        assert_eq!(active_model.as_deref(), Some("gpt-4o"));
        assert_eq!(
            store.get_config("active_model").unwrap().as_deref(),
            Some("gpt-4o")
        );
    }

    #[test]
    fn logout_clears_credential_and_active_selection() {
        let store = temp_store();
        let runtime = PiAgentRuntime::start(store.clone());
        let _ = runtime.try_recv();

        runtime
            .send(WorkerRequest::SetApiKey {
                provider_id: "openai".to_owned(),
                api_key: "sk-test-key".to_owned(),
            })
            .unwrap();
        let _ = recv_until(&runtime, Duration::from_secs(10), |e| {
            matches!(e, WorkerEvent::ProvidersSnapshot { .. })
        });

        runtime
            .send(WorkerRequest::Logout {
                provider_id: "openai".to_owned(),
            })
            .unwrap();
        let event = recv_until(&runtime, Duration::from_secs(10), |e| {
            matches!(e, WorkerEvent::ConfigChanged { .. })
        });
        let Some(WorkerEvent::ConfigChanged {
            active_provider,
            active_model,
        }) = event
        else {
            panic!("expected ConfigChanged after Logout");
        };
        assert!(active_provider.is_none(), "active provider should clear");
        assert!(active_model.is_none(), "active model should clear");
        assert!(store.read_provider_credential("openai").unwrap().is_none());
    }

    #[test]
    fn prompt_without_provider_fails_fast_with_helpful_error() {
        let runtime = PiAgentRuntime::start(temp_store());
        let _ = runtime.try_recv();

        runtime
            .send(WorkerRequest::Prompt {
                request_id: "req-1".to_owned(),
                session_id: "sess-1".to_owned(),
                content: "hello".to_owned(),
            })
            .unwrap();

        let event = recv_until(&runtime, Duration::from_secs(10), |e| {
            matches!(e, WorkerEvent::RunFailed { .. })
        });
        let Some(WorkerEvent::RunFailed {
            request_id, error, ..
        }) = event
        else {
            panic!("expected RunFailed for a prompt with no provider");
        };
        assert_eq!(request_id, "req-1");
        assert!(
            error.contains("no AI provider configured"),
            "unexpected error: {error}"
        );
    }

    /// A stored provider id that the current pi catalog no longer knows (e.g.
    /// saved by an older build, then the dependency upgraded) must surface a
    /// prompt failure — never wedge the request lock forever.
    #[test]
    fn prompt_with_stale_provider_id_fails_instead_of_wedging() {
        let store = temp_store();
        store
            .set_config("active_provider", "opencode-go")
            .expect("set provider");
        store
            .set_config("active_model", "deepseek-v4-flash")
            .expect("set model");
        store
            .write_provider_credential(
                "opencode-go",
                &ProviderCredential {
                    kind: "api_key".to_owned(),
                    key: "sk-test".to_owned(),
                },
            )
            .expect("write credential");

        let runtime = PiAgentRuntime::start(store);
        let _ = runtime.try_recv();

        runtime
            .send(WorkerRequest::Prompt {
                request_id: "req-stale".to_owned(),
                session_id: "sess-1".to_owned(),
                content: "hello".to_owned(),
            })
            .unwrap();

        // Generous timeout: this is the exact scenario that wedged the user's
        // app — if pi hangs resolving the unknown provider, this test hangs
        // with it and the timeout failure names the bug.
        let event = recv_until(&runtime, Duration::from_secs(90), |e| {
            matches!(e, WorkerEvent::RunFailed { .. })
        });
        let Some(WorkerEvent::RunFailed { error, .. }) = event else {
            panic!("prompt with a stale provider id must RunFailed, not wedge");
        };
        assert!(
            !error.contains("no AI provider configured"),
            "a provider IS configured (stale id) — the error should name the model/provider problem: {error}"
        );
    }

    /// Saving an API key must leave the agent ready to prompt: provider
    /// active AND a model selected. Without the auto-selected model every
    /// prompt died with "no model configured" — the "AI is not working" bug.
    #[test]
    fn set_api_key_activates_provider_and_selects_a_model() {
        let store = temp_store();
        let runtime = PiAgentRuntime::start(store);
        let _ = runtime.try_recv();

        runtime
            .send(WorkerRequest::SetApiKey {
                provider_id: "openai".to_owned(),
                api_key: "sk-test".to_owned(),
            })
            .unwrap();

        let event = recv_until(&runtime, Duration::from_secs(10), |e| {
            matches!(e, WorkerEvent::ConfigChanged { .. })
        });
        let Some(WorkerEvent::ConfigChanged {
            active_provider,
            active_model,
        }) = event
        else {
            panic!("expected ConfigChanged after SetApiKey");
        };
        assert_eq!(active_provider.as_deref(), Some("openai"));
        let model = active_model.expect("a model must be auto-selected");
        assert!(!model.is_empty(), "auto-selected model must not be empty");
    }

    #[test]
    fn cancel_without_active_run_is_quiet() {
        let runtime = PiAgentRuntime::start(temp_store());
        let _ = runtime.try_recv();

        runtime
            .send(WorkerRequest::Cancel {
                request_id: "ghost".to_owned(),
                session_id: String::new(),
            })
            .unwrap();

        let event = recv_until(&runtime, Duration::from_millis(300), |e| {
            matches!(e, WorkerEvent::RunFailed { .. })
        });
        assert!(
            event.is_none(),
            "cancel with no active run must not fabricate a failure"
        );
    }

    #[test]
    fn cancel_is_not_blocked_by_a_busy_worker() {
        let runtime = PiAgentRuntime::start(temp_store());
        let _ = runtime.try_recv();

        // Simulate a long-running prompt holding the request lock.
        let guard = runtime.request_lock.lock().unwrap();
        let started = Instant::now();
        runtime
            .send(WorkerRequest::Cancel {
                request_id: "req-busy".to_owned(),
                session_id: String::new(),
            })
            .unwrap();
        let elapsed = started.elapsed();
        drop(guard);

        assert!(
            elapsed < Duration::from_millis(500),
            "cancel must not queue behind the request lock (took {elapsed:?})"
        );
    }
}
