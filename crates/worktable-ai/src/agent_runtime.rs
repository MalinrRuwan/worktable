//! The embedded AI agent runtime, built on [rig](https://rig.rs).
//!
//! Worktable used to embed `pi_agent_rust`; this module is its rig
//! replacement. `rig` runs on Tokio like the rest of the app, so a request is
//! a task on the shared runtime instead of a dedicated OS thread with its own
//! executor.
//!
//! Responsibilities:
//!
//! - own the [`SqliteStore`] view of provider credentials and config;
//! - build the right rig client for the active [`ProviderKind`];
//! - run a streaming agent (preamble + `search_knowledge` on native) and fan
//!   its text, reasoning, tool, and lifecycle events into [`WorkerEvent`]s;
//! - keep per-session conversation history from the run transcript
//!   (`PromptResponse::messages`), so tool results survive into the next turn;
//! - support cancellation without queuing behind the run it cancels.
//!
//! Provider catalog and model lists live in [`crate::providers`]; the
//! OpenCode gateways (Go and Zen) and their per-model client routing are in
//! [`crate::opencode`].

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, Sender, channel},
    },
};

use anyhow::anyhow;
use futures::StreamExt as _;
use rig::agent::MultiTurnStreamItem;
use rig::completion::{AssistantContent, Message};
use rig::prelude::*;
use rig::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingChat};
use worktable_db::{ProviderCredential, SqliteStore};
use worktable_events::{ProviderGroup, ProviderInfo, ProvidersSnapshot};

use crate::{
    chatgpt, opencode,
    providers::{self, ProviderKind, ProviderSpec},
    worker_protocol::{KnowledgeCitation, WorkerEvent, WorkerRequest},
};

/// Error string used when a run ends because the user cancelled it. The UI
/// matches on this to avoid surfacing cancellations as failures.
pub const ABORTED_BY_USER: &str = "aborted by user";

/// System prompt for the assistant. Kept short: the app's own UI copy and the
/// `search_knowledge` tool description carry the domain detail.
const SYSTEM_PREAMBLE: &str = "You are Worktable's assistant, embedded in a personal notes \
    app. Be concise and practical. When the user asks about their saved notes, links, or \
    images, search the knowledge base with the `search_knowledge` tool before answering, \
    and never answer about their notes from memory. Every entry you use MUST be cited \
    inline with its [n] marker from the tool's numbering, placed right after the claim it \
    supports; answers grounded in the knowledge base without [n] markers are wrong. \
    Answer in Markdown.";

/// How many messages of a session's transcript to keep in memory.
const MAX_HISTORY_MESSAGES: usize = 100;

/// Entries per model call when enriching the knowledge graph.
const ENRICH_BATCH_SIZE: usize = 8;

/// Instructions for the knowledge enrichment pass. The model must answer with
/// JSON only so the parser can stay strict.
const KNOWLEDGE_PREAMBLE: &str = "You build a personal knowledge graph from a user's \
    saved notes, links, and images. For each entry, return 2 to 5 short lowercase topic \
    keywords or phrases describing what it is about. Reuse the same vocabulary across \
    entries about the same subject so the graph links them. Reply with ONLY a JSON \
    array, no prose and no code fences: [{\"id\":\"<entry id>\",\"topics\":[\"…\"]}]";

/// The active provider/model/key resolved from the store.
struct ActiveProviderConfig {
    spec: &'static ProviderSpec,
    key: String,
    model: String,
    /// Stable-ish session id for providers that route by conversation.
    session: String,
    /// Gateway + dialect for the OpenCode provider; `None` for providers with
    /// a single fixed client.
    route: Option<opencode::Route>,
}

/// Resolve the active provider, its credential, and model, with the same
/// user-facing error messages a prompt uses.
fn resolve_active_provider(store: &SqliteStore) -> Result<ActiveProviderConfig, String> {
    let provider_id = match store.get_config("active_provider") {
        Ok(Some(provider)) if !provider.is_empty() => provider,
        _ => return Err("no AI provider configured; open Settings to set one up".to_owned()),
    };
    let model = match store.get_config("active_model") {
        Ok(Some(model)) if !model.is_empty() => model,
        _ => {
            return Err(format!("no model configured for provider {provider_id}"));
        }
    };
    let Some(spec) = providers::spec(&provider_id) else {
        return Err(format!(
            "provider '{provider_id}' is not in this build's catalog; pick a provider in Settings"
        ));
    };
    // OpenCode serves two catalogs through one key; the enabled services
    // decide which gateway (and dialect) serves the selected model.
    let route = if spec.kind == ProviderKind::OpenCode {
        let models = crate::model_state::read(store, &provider_id).models;
        match models
            .iter()
            .find(|row| row.id == model)
            .and_then(|row| opencode::route(row, opencode::Services::from_store(store)))
        {
            Some(route) => Some(route),
            None => {
                return Err(format!(
                    "model '{model}' is not offered by the enabled OpenCode services; \
                     pick a model in Settings"
                ));
            }
        }
    } else {
        None
    };
    let key = store
        .read_provider_credential(&provider_id)
        .ok()
        .flatten()
        .filter(|credential| spec.kind != ProviderKind::ChatGpt || credential.kind == "oauth")
        .map(|credential| credential.key);
    let Some(key) = key.filter(|key| !key.trim().is_empty()) else {
        if spec.kind == ProviderKind::ChatGpt {
            return Err(chatgpt::SIGN_IN_AGAIN.to_owned());
        }
        return Err(format!(
            "no API key is stored for {provider_id}; add one in Settings"
        ));
    };
    Ok(ActiveProviderConfig {
        spec,
        key,
        model,
        session: uuid::Uuid::new_v4().to_string(),
        route,
    })
}

async fn refresh_subscription(
    store: &SqliteStore,
    config: &mut ActiveProviderConfig,
) -> Result<(), String> {
    if config.spec.kind == ProviderKind::ChatGpt {
        config.key = chatgpt::auth_context(store).await?.record_json();
    }
    Ok(())
}

/// Drop reasoning parts from a transcript before replaying it to a provider.
///
/// Reasoning is model output, not input: OpenAI-compatible endpoints reject
/// (or worse, stall on) assistant reasoning blocks sent back on the next
/// turn. The UI still receives reasoning live through `AgentThoughtDelta`.
fn sanitize_history(messages: Vec<Message>) -> Vec<Message> {
    messages
        .into_iter()
        .filter_map(|message| match message {
            Message::Assistant { id, content } => {
                let content: Vec<AssistantContent> = content
                    .into_iter()
                    .filter(|part| !matches!(part, AssistantContent::Reasoning(_)))
                    .collect();
                (!content.is_empty()).then_some(Message::Assistant { id, content })
            }
            other => Some(other),
        })
        .collect()
}

fn session_history(
    store: &SqliteStore,
    memory: &HashMap<String, Vec<Message>>,
    session_id: &str,
) -> anyhow::Result<Vec<Message>> {
    let messages = match memory.get(session_id) {
        Some(messages) => messages.clone(),
        None => store
            .load_session_history(session_id)?
            .map(|json| serde_json::from_str::<Vec<Message>>(&json))
            .transpose()?
            .unwrap_or_default(),
    };
    let mut messages = sanitize_history(messages);
    if messages.len() > MAX_HISTORY_MESSAGES {
        messages.drain(0..messages.len() - MAX_HISTORY_MESSAGES);
    }
    Ok(messages)
}

/// Extract topics for `entries` with the active provider's model. Each batch
/// is one model call; the graph is updated by the caller.
pub(crate) async fn enrich_topics(
    store: &SqliteStore,
    entries: Vec<worktable_db::Entry>,
) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut config = resolve_active_provider(store).map_err(anyhow::Error::msg)?;
    refresh_subscription(store, &mut config)
        .await
        .map_err(anyhow::Error::msg)?;
    let agent = build_agent(
        config.spec,
        &config.key,
        &config.model,
        &config.session,
        config.route,
        None,
    )?;
    let mut topics = Vec::new();
    for batch in entries.chunks(ENRICH_BATCH_SIZE) {
        topics.extend(enrich_with_agent(&agent, batch).await?);
    }
    Ok(topics)
}

/// One model call: send the entries, parse the JSON topic list.
async fn enrich_with_agent(
    agent: &rig::agent::Agent,
    entries: &[worktable_db::Entry],
) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    let payload: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            serde_json::json!({
                "id": entry.id,
                "title": entry.title,
                "content": entry.content.chars().take(600).collect::<String>(),
            })
        })
        .collect();
    let prompt = format!(
        "{KNOWLEDGE_PREAMBLE}\n\nEntries:\n{}",
        serde_json::to_string(&payload)?
    );
    let output = agent
        .prompt(prompt)
        .await
        .map_err(|error| anyhow!("knowledge enrichment failed: {error}"))?;
    Ok(parse_topic_json(&output))
}

/// Parse the model's topic JSON. Tolerates code fences and a `topics` string
/// instead of an array; unknown shapes are dropped rather than failing the
/// whole build.
fn parse_topic_json(text: &str) -> Vec<(String, Vec<String>)> {
    let Some(start) = text.find('[') else {
        return Vec::new();
    };
    let Some(end) = text.rfind(']') else {
        return Vec::new();
    };
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(&text[start..=end]) else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?.to_owned();
            let topics: Vec<String> = match item.get("topics")? {
                serde_json::Value::Array(values) => values
                    .iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect(),
                serde_json::Value::String(value) => value
                    .split(',')
                    .map(|topic| topic.trim().to_owned())
                    .filter(|topic| !topic.is_empty())
                    .collect(),
                _ => return None,
            };
            (!topics.is_empty()).then_some((id, topics))
        })
        .collect()
}

/// Model-call budget for one prompt. A run starts with the prompt, may call
/// `search_knowledge` (tools execute inside the budget), then needs at least
/// one more model call to answer — rig's implicit budget is a single call, so
/// without this a tool call can never produce a final answer.
const MAX_MODEL_TURNS: usize = 8;

/// Tool type handed to the agent: the Helix search tool on native, nothing on
/// wasm (Helix is native-only).
#[cfg(not(target_arch = "wasm32"))]
type ProvidedTool = crate::helix_tool::SearchKnowledgeTool;
#[cfg(target_arch = "wasm32")]
type ProvidedTool = ();

/// Attach the tool when one exists. The macro keeps the agent builder's two
/// typestates (`NoToolConfig` and `WithBuilderTools`) from leaking into the
/// provider match, and compiles away on wasm where no tool exists.
#[cfg(not(target_arch = "wasm32"))]
macro_rules! finish_agent {
    ($builder:expr, $tool:expr) => {
        match $tool {
            Some(tool) => $builder.tool(tool).build(),
            None => $builder.build(),
        }
    };
}
#[cfg(target_arch = "wasm32")]
macro_rules! finish_agent {
    ($builder:expr, $tool:expr) => {{
        let _ = $tool;
        $builder.build()
    }};
}

/// One in-flight run that `Cancel` can reach without waiting for a lock.
struct ActiveRun {
    request_id: String,
    session_id: String,
    cancel: Arc<AtomicBool>,
    /// Wakes the in-flight stream the moment a cancel arrives, so a stalled
    /// provider response is dropped instead of waiting for the next chunk.
    notify: Arc<tokio::sync::Notify>,
}

impl ActiveRun {
    fn matches(&self, cancel: &Arc<AtomicBool>) -> bool {
        Arc::ptr_eq(&self.cancel, cancel)
    }
}

struct ActiveLogin {
    cancel: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl ActiveLogin {
    fn matches(&self, cancel: &Arc<AtomicBool>) -> bool {
        Arc::ptr_eq(&self.cancel, cancel) && !cancel.load(Ordering::SeqCst)
    }
}

/// The embedded agent runtime.
///
/// `send` is non-blocking: prompts are spawned on the Tokio handle the app
/// already owns, and progress arrives through `try_recv`/the runtime event
/// pump exactly like the previous embedded agent.
pub struct AgentRuntime {
    store: SqliteStore,
    tokio: tokio::runtime::Handle,
    /// Unbounded: the worker must never block on a slow UI consumer — a full
    /// bounded channel stalled long streams mid-answer.
    events_tx: Sender<WorkerEvent>,
    events_rx: Mutex<Receiver<WorkerEvent>>,
    active: Arc<Mutex<Option<ActiveRun>>>,
    login: Arc<Mutex<Option<ActiveLogin>>>,
    /// Conversation transcript per session, seeded by the previous run's
    /// `PromptResponse::messages`.
    history: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    /// Stable id for the OpenCode Go `x-opencode-session` header when a
    /// request arrives without a session id.
    default_session: String,
    model_loader: crate::ModelLoader,
    model_jobs: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>,
}

impl AgentRuntime {
    /// Create the runtime on `tokio`'s executor. A `Ready` event is emitted
    /// once so the event pump reports the AI worker as ready.
    pub fn start(store: SqliteStore, tokio: tokio::runtime::Handle) -> Self {
        Self::start_with_model_loader(
            store,
            tokio,
            Arc::new(|store, provider| Box::pin(crate::model_catalog::fetch(store, provider))),
        )
    }

    pub fn start_with_model_loader(
        store: SqliteStore,
        tokio: tokio::runtime::Handle,
        model_loader: crate::ModelLoader,
    ) -> Self {
        let (events_tx, events_rx) = channel();
        let runtime = Self {
            store,
            tokio,
            events_tx,
            events_rx: Mutex::new(events_rx),
            active: Arc::new(Mutex::new(None)),
            login: Arc::new(Mutex::new(None)),
            history: Arc::new(Mutex::new(HashMap::new())),
            default_session: uuid::Uuid::new_v4().to_string(),
            model_loader,
            model_jobs: Arc::new(Mutex::new(HashMap::new())),
        };
        let _ = runtime.events_tx.send(WorkerEvent::Ready);
        runtime
    }

    pub fn try_recv(&self) -> Option<WorkerEvent> {
        self.events_rx.lock().unwrap().try_recv().ok()
    }

    /// Dispatch a request.
    ///
    /// `Cancel` is handled synchronously on the caller's thread: queueing it
    /// behind the run it is meant to abort would make cancellation a no-op.
    /// Store-only requests (catalog/config) run inline; prompts are spawned.
    pub fn send(&self, request: WorkerRequest) -> anyhow::Result<()> {
        match request {
            WorkerRequest::Cancel {
                request_id,
                session_id,
            } => {
                self.cancel(&request_id, &session_id);
            }
            WorkerRequest::Prompt {
                request_id,
                session_id,
                content,
            } => self.start_prompt(request_id, session_id, content)?,
            WorkerRequest::ListProviders => {
                self.refresh_models(true);
                self.emit_snapshot();
            }
            WorkerRequest::SetApiKey {
                provider_id,
                api_key,
            } => self.set_api_key(&provider_id, &api_key),
            WorkerRequest::SetModel {
                provider_id,
                model_id,
            } => {
                if !crate::model_state::read(&self.store, &provider_id)
                    .models
                    .iter()
                    .any(|model| model.id == model_id)
                {
                    let _ = self.events_tx.send(WorkerEvent::WorkerError {
                        error: "This model is no longer available. Refresh models in Settings and choose again."
                            .to_owned(),
                    });
                    self.emit_snapshot();
                    return Ok(());
                }
                if self
                    .store
                    .set_config("active_provider", &provider_id)
                    .is_ok()
                {
                    let _ = self.store.set_config("active_model", &model_id);
                }
                self.emit_config_changed();
            }
            WorkerRequest::SetProviderGroup {
                provider_id,
                group_id,
                enabled,
            } => self.set_provider_group(&provider_id, &group_id, enabled),
            WorkerRequest::Logout { provider_id } => self.logout(&provider_id),
            WorkerRequest::LoginOAuth { provider_id } => {
                if provider_id == chatgpt::ID {
                    self.invalidate_models(chatgpt::ID);
                    self.start_subscription_login();
                } else {
                    let _ = self.events_tx.send(WorkerEvent::LoginResult {
                        provider_id,
                        ok: false,
                        error: Some(
                            "OAuth login is not available; configure an API key instead".to_owned(),
                        ),
                    });
                }
            }
            WorkerRequest::CancelLogin { provider_id } if provider_id == chatgpt::ID => {
                self.cancel_subscription_login(true);
            }
            WorkerRequest::CancelLogin { .. } | WorkerRequest::AnswerAuthPrompt { .. } => {
                let _ = self.events_tx.send(WorkerEvent::WorkerError {
                    error: "unsupported by the embedded agent".to_owned(),
                });
            }
            WorkerRequest::Shutdown => {}
        }
        Ok(())
    }

    fn invalidate_models(&self, provider_id: &str) {
        let mut jobs = self.model_jobs.lock().unwrap();
        if let Some(job) = jobs.remove(provider_id) {
            job.abort();
        }
        if crate::model_state::invalidate(&self.store, provider_id).is_err() {
            let _ = self.events_tx.send(WorkerEvent::WorkerError {
                error: "Couldn't reset the model list. Try again in Settings.".to_owned(),
            });
        }
    }

    fn refresh_models(&self, force: bool) {
        for spec in providers::PROVIDERS {
            let credential = self.store.read_provider_credential(spec.id).ok().flatten();
            let configured = credential.is_some_and(|credential| {
                !credential.key.is_empty()
                    && ((spec.supports_api_key() && credential.kind == "api_key")
                        || (spec.supports_oauth() && credential.kind == "oauth"))
            });
            if !configured {
                continue;
            }
            let mut jobs = self.model_jobs.lock().unwrap();
            let mut state = crate::model_state::read(&self.store, spec.id);
            if jobs.contains_key(spec.id)
                || (!force && !state.models.is_empty() && state.error.is_none())
            {
                continue;
            }
            state.loading = true;
            state.error = None;
            if crate::model_state::write(&self.store, spec.id, &state).is_err() {
                continue;
            }
            let revision = state.revision;
            let provider_id = spec.id.to_owned();
            let store = self.store.clone();
            let loader = self.model_loader.clone();
            let tx = self.events_tx.clone();
            let current_jobs = self.model_jobs.clone();
            let task_id = provider_id.clone();
            let task = self.tokio.spawn(async move {
                let result = loader(store.clone(), task_id.clone()).await;
                let mut jobs = current_jobs.lock().unwrap();
                let mut state = crate::model_state::read(&store, &task_id);
                if state.revision != revision || !jobs.contains_key(&task_id) {
                    return;
                }
                jobs.remove(&task_id);
                state.loading = false;
                match result {
                    Ok(fetched) => {
                        state.models = fetched.models;
                        state.error = fetched.warning;
                    }
                    Err(error) => state.error = Some(error),
                }
                if crate::model_state::write(&store, &task_id, &state).is_err() {
                    return;
                }
                // Keep the selected id only if the authenticated API still
                // offers it. Otherwise default within this provider, not from
                // a second static catalog.
                if store
                    .get_config("active_provider")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(task_id.as_str())
                    && !state.models.iter().any(|model| {
                        store.get_config("active_model").ok().flatten().as_deref()
                            == Some(model.id.as_str())
                    })
                {
                    let model = state
                        .models
                        .first()
                        .map(|model| model.id.as_str())
                        .unwrap_or("");
                    let _ = store.set_config("active_model", model);
                    let _ = tx.send(WorkerEvent::ConfigChanged {
                        active_provider: Some(task_id.clone()),
                        active_model: (!model.is_empty()).then(|| model.to_owned()),
                    });
                }
                if let Ok(snapshot) = build_snapshot(&store) {
                    let _ = tx.send(WorkerEvent::ProvidersSnapshot { snapshot });
                }
            });
            jobs.insert(provider_id, task.abort_handle());
        }
    }

    fn cancel_subscription_login(&self, report: bool) {
        let mut current = self.login.lock().unwrap();
        if let Some(login) = current.take() {
            login.cancel.store(true, Ordering::SeqCst);
            login.notify.notify_one();
            if report {
                let _ = self.events_tx.send(WorkerEvent::LoginResult {
                    provider_id: chatgpt::ID.to_owned(),
                    ok: false,
                    error: Some("ChatGPT sign-in cancelled".to_owned()),
                });
            }
        }
    }

    fn start_subscription_login(&self) {
        let cancel = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        {
            let mut current = self.login.lock().unwrap();
            if let Some(previous) = current.take() {
                previous.cancel.store(true, Ordering::SeqCst);
                previous.notify.notify_one();
            }
            *current = Some(ActiveLogin {
                cancel: cancel.clone(),
                notify: notify.clone(),
            });
        }
        let login = self.login.clone();
        let callback_login = login.clone();
        let callback_cancel = cancel.clone();
        let tx = self.events_tx.clone();
        let callback_tx = tx.clone();
        let store = self.store.clone();
        self.tokio.spawn(async move {
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            let result = tokio::select! {
                biased;
                _ = notify.notified() => return,
                result = chatgpt::login(move |user_code, verification_uri| {
                    let current = callback_login.lock().unwrap();
                    if current.as_ref().is_some_and(|login| login.matches(&callback_cancel)) {
                        let _ = callback_tx.send(WorkerEvent::AuthNotify {
                            provider_id: chatgpt::ID.to_owned(),
                            notify: worktable_events::AuthNotifyKind::DeviceCode {
                                user_code,
                                verification_uri,
                                expires_in_seconds: Some(15 * 60),
                            },
                        });
                    }
                }) => result,
            };
            // Hold identity through persistence and the final event: cancelling,
            // replacing login, or logging out invalidates this result atomically.
            let mut current = login.lock().unwrap();
            if !current.as_ref().is_some_and(|login| login.matches(&cancel)) {
                return;
            }
            let result = result.and_then(|key| {
                let _guard = chatgpt::CREDENTIAL_LOCK.lock().unwrap();
                store
                    .write_provider_credential(
                        chatgpt::ID,
                        &ProviderCredential {
                            kind: "oauth".to_owned(),
                            key,
                        },
                    )
                    .and_then(|()| store.set_config("active_provider", chatgpt::ID))
                    .and_then(|()| store.set_config("active_model", ""))
                    .map_err(|_| {
                        "Could not save ChatGPT authorization. Sign in again in Settings."
                            .to_owned()
                    })
            });
            *current = None;
            let _ = tx.send(WorkerEvent::LoginResult {
                provider_id: chatgpt::ID.to_owned(),
                ok: result.is_ok(),
                error: result.err(),
            });
            // Model discovery owns activation/selection. Never pick an API-key
            // model for a subscription simply because login completed.
            if let Ok(snapshot) = build_snapshot(&store) {
                let _ = tx.send(WorkerEvent::ProvidersSnapshot { snapshot });
            }
        });
    }

    /// Cancel the in-flight run, if any. The run's task notices the flag and
    /// exits quietly, because the failure event is emitted here.
    fn cancel(&self, request_id: &str, session_id: &str) {
        let Some(run) = self.active.lock().unwrap().take() else {
            return;
        };
        run.cancel.store(true, Ordering::SeqCst);
        // Wake the stream immediately: dropping it aborts the HTTP request.
        run.notify.notify_one();
        let _ = self.events_tx.send(WorkerEvent::RunFailed {
            request_id: if request_id.is_empty() {
                run.request_id
            } else {
                request_id.to_owned()
            },
            session_id: if session_id.is_empty() {
                run.session_id
            } else {
                session_id.to_owned()
            },
            error: ABORTED_BY_USER.to_owned(),
        });
    }

    fn start_prompt(
        &self,
        request_id: String,
        session_id: String,
        content: String,
    ) -> anyhow::Result<()> {
        if self.active.lock().unwrap().is_some() {
            let _ = self.events_tx.send(WorkerEvent::RunFailed {
                request_id,
                session_id,
                error: "another prompt is already running".to_owned(),
            });
            return Ok(());
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        *self.active.lock().unwrap() = Some(ActiveRun {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            cancel: Arc::clone(&cancel),
            notify: Arc::clone(&notify),
        });

        let store = self.store.clone();
        let tx = self.events_tx.clone();
        let history = Arc::clone(&self.history);
        let active = Arc::clone(&self.active);
        let default_session = self.default_session.clone();
        self.tokio.spawn(async move {
            run_prompt(
                store,
                tx,
                history,
                active,
                cancel,
                notify,
                default_session,
                request_id,
                session_id,
                content,
            )
            .await;
        });

        Ok(())
    }

    fn set_api_key(&self, provider_id: &str, api_key: &str) {
        if provider_id == chatgpt::ID {
            let _ = self.events_tx.send(WorkerEvent::WorkerError {
                error: "ChatGPT subscription uses sign-in, not an API key. Sign in in Settings → Providers.".to_owned(),
            });
            return;
        }
        let credential = ProviderCredential {
            kind: "api_key".to_owned(),
            key: api_key.to_owned(),
        };
        self.invalidate_models(provider_id);
        if let Err(error) = self
            .store
            .write_provider_credential(provider_id, &credential)
        {
            let _ = self.events_tx.send(WorkerEvent::WorkerError {
                error: format!("failed to store API key: {error}"),
            });
            return;
        }

        // Saving a key activates that provider. Updating only its model while
        // retaining another provider would leave an invalid selected pair.
        let _ = self.store.set_config("active_provider", provider_id);
        let _ = self.store.set_config("active_model", "");
        self.refresh_models(false);
        self.emit_config_changed();
    }

    /// Enable or disable one of OpenCode's model catalogs. Disabling a
    /// catalog can strand the active model, so the selection is re-defaulted
    /// to the first offered model (or cleared when nothing is left).
    fn set_provider_group(&self, provider_id: &str, group_id: &str, enabled: bool) {
        if provider_id != opencode::ID {
            let _ = self.events_tx.send(WorkerEvent::WorkerError {
                error: format!("provider '{provider_id}' has no model groups"),
            });
            return;
        }
        let Some(service) = opencode::Service::from_id(group_id) else {
            let _ = self.events_tx.send(WorkerEvent::WorkerError {
                error: format!("unknown OpenCode service '{group_id}'"),
            });
            return;
        };
        let services = opencode::Services::from_store(&self.store).with(service, enabled);
        self.invalidate_models(provider_id);
        if let Err(error) = services.write_to_store(&self.store) {
            let _ = self.events_tx.send(WorkerEvent::WorkerError {
                error: format!("failed to store the OpenCode services: {error}"),
            });
            return;
        }

        let active_provider = self.store.get_config("active_provider").ok().flatten();
        if active_provider.as_deref() == Some(opencode::ID) {
            let _ = self.store.set_config("active_model", "");
            if !services.go && !services.zen {
                let _ = self.store.set_config("active_provider", "");
            }
        }

        self.refresh_models(false);
        self.emit_config_changed();
    }

    fn logout(&self, provider_id: &str) {
        self.invalidate_models(provider_id);
        if provider_id == chatgpt::ID {
            // Same lock order as login completion. Keep it until DB deletion,
            // so no successful late login can restore a logged-out credential.
            let mut current = self.login.lock().unwrap();
            if let Some(login) = current.take() {
                login.cancel.store(true, Ordering::SeqCst);
                login.notify.notify_one();
            }
            let _guard = chatgpt::CREDENTIAL_LOCK.lock().unwrap();
            let _ = self.store.delete_provider_credential(provider_id);
        } else {
            let _ = self.store.delete_provider_credential(provider_id);
        }
        if self
            .store
            .get_config("active_provider")
            .ok()
            .flatten()
            .as_deref()
            == Some(provider_id)
        {
            let _ = self.store.set_config("active_provider", "");
            let _ = self.store.set_config("active_model", "");
        }
        self.emit_config_changed();
    }

    fn emit_snapshot(&self) {
        let snapshot = build_snapshot(&self.store).unwrap_or_else(|error| {
            let _ = self.events_tx.send(WorkerEvent::WorkerError {
                error: format!("failed to list providers: {error}"),
            });
            empty_snapshot()
        });
        let _ = self
            .events_tx
            .send(WorkerEvent::ProvidersSnapshot { snapshot });
    }

    fn emit_config_changed(&self) {
        let active_provider = self.store.get_config("active_provider").ok().flatten();
        let active_model = self.store.get_config("active_model").ok().flatten();
        let _ = self.events_tx.send(WorkerEvent::ConfigChanged {
            active_provider: active_provider.filter(|value| !value.is_empty()),
            active_model: active_model.filter(|value| !value.is_empty()),
        });
        let snapshot = build_snapshot(&self.store).unwrap_or_else(|_| empty_snapshot());
        let _ = self
            .events_tx
            .send(WorkerEvent::ProvidersSnapshot { snapshot });
    }

    pub fn shutdown(&self) {
        self.cancel_subscription_login(false);
        for (_, job) in self.model_jobs.lock().unwrap().drain() {
            job.abort();
        }
        if let Some(run) = self.active.lock().unwrap().take() {
            run.cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// Run one streaming prompt to completion.
#[allow(clippy::too_many_arguments)]
async fn run_prompt(
    store: SqliteStore,
    tx: Sender<WorkerEvent>,
    history: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    active: Arc<Mutex<Option<ActiveRun>>>,
    cancel: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
    default_session: String,
    request_id: String,
    session_id: String,
    content: String,
) {
    // Resolve the active provider/model and its credential.
    let mut config = match resolve_active_provider(&store) {
        Ok(config) => config,
        Err(error) => {
            fail(&tx, &active, &cancel, &request_id, &session_id, error);
            return;
        }
    };

    if cancel.load(Ordering::SeqCst) {
        return;
    }
    let refreshed = tokio::select! {
        biased;
        _ = notify.notified() => return,
        result = refresh_subscription(&store, &mut config) => result,
    };
    if let Err(error) = refreshed {
        fail(&tx, &active, &cancel, &request_id, &session_id, error);
        return;
    }

    // Build the agent for this provider's rig client family. The collector is
    // shared with the tool so the run can emit the search hits as citations.
    #[cfg(not(target_arch = "wasm32"))]
    let citations = crate::helix_tool::CitationCollector::new();
    #[cfg(not(target_arch = "wasm32"))]
    let tool =
        Some(crate::helix_tool::SearchKnowledgeTool::new().with_citations(citations.clone()));
    #[cfg(target_arch = "wasm32")]
    let tool: Option<ProvidedTool> = None;

    let routing_session = if session_id.is_empty() {
        default_session.as_str()
    } else {
        session_id.as_str()
    };
    let agent = match build_agent(
        config.spec,
        &config.key,
        &config.model,
        routing_session,
        config.route,
        tool,
    ) {
        Ok(agent) => agent,
        Err(error) => {
            fail(
                &tx,
                &active,
                &cancel,
                &request_id,
                &session_id,
                format!("failed to start the agent: {error}"),
            );
            return;
        }
    };

    let prompt = Message::user(content.clone());
    let prior = match session_history(&store, &history.lock().unwrap(), &session_id) {
        Ok(messages) => messages,
        Err(error) => {
            fail(
                &tx,
                &active,
                &cancel,
                &request_id,
                &session_id,
                format!("Couldn't load this chat's context: {error}"),
            );
            return;
        }
    };
    let mut stream = agent.stream_chat(prompt.clone(), prior.clone()).await;

    let mut final_messages: Option<Vec<Message>> = None;
    loop {
        if cancel.load(Ordering::SeqCst) {
            // `cancel` already emitted the failure event; leave quietly.
            clear_active(&active, &cancel);
            return;
        }
        // Cancellation interrupts an in-flight provider call: the select
        // wakes on the notify and dropping the stream aborts the request.
        let next = tokio::select! {
            item = stream.next() => item,
            _ = notify.notified() => {
                clear_active(&active, &cancel);
                return;
            }
        };
        let Some(item) = next else {
            break;
        };
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => match content {
                StreamedAssistantContent::Text(text) => {
                    let _ = tx.send(WorkerEvent::AgentMessageDelta {
                        request_id: request_id.clone(),
                        session_id: session_id.clone(),
                        delta: text.text,
                    });
                }
                StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                    let _ = tx.send(WorkerEvent::AgentThoughtDelta {
                        request_id: request_id.clone(),
                        session_id: session_id.clone(),
                        delta: reasoning,
                    });
                }
                StreamedAssistantContent::ToolCall { tool_call, .. } => {
                    let _ = tx.send(WorkerEvent::ToolStarted {
                        request_id: request_id.clone(),
                        session_id: session_id.clone(),
                        tool_call_id: tool_call.id.to_string(),
                        name: tool_call.function.name,
                    });
                }
                _ => {}
            },
            Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                tool_result,
                ..
            })) => {
                let _ = tx.send(WorkerEvent::ToolFinished {
                    request_id: request_id.clone(),
                    session_id: session_id.clone(),
                    tool_call_id: tool_result.call.to_string(),
                    name: tool_result.name,
                });
            }
            Ok(MultiTurnStreamItem::FinalResponse(response)) => {
                final_messages = response.messages;
            }
            Ok(_) => {}
            Err(error) => {
                fail(
                    &tx,
                    &active,
                    &cancel,
                    &request_id,
                    &session_id,
                    error.to_string(),
                );
                return;
            }
        }
    }

    // Keep the run's own transcript (it includes tool calls and results) so
    // the next turn can see them; fall back to the prompt when a provider
    // returned no transcript.
    let mut transcript = sanitize_history(final_messages.unwrap_or_else(|| {
        prior
            .into_iter()
            .chain([prompt.clone()])
            .collect::<Vec<Message>>()
    }));
    if transcript.len() > MAX_HISTORY_MESSAGES {
        let excess = transcript.len() - MAX_HISTORY_MESSAGES;
        transcript.drain(0..excess);
    }
    let saved = serde_json::to_string(&transcript)
        .map_err(anyhow::Error::from)
        .and_then(|json| store.save_session_history(&session_id, &json));
    if let Err(error) = saved {
        fail(
            &tx,
            &active,
            &cancel,
            &request_id,
            &session_id,
            format!("Couldn't save this chat's context: {error}"),
        );
        return;
    }
    history
        .lock()
        .unwrap()
        .insert(session_id.clone(), transcript);

    // Surface the collected knowledge citations with the answer they annotate.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let collected: Vec<KnowledgeCitation> = citations.take();
        if !collected.is_empty() {
            let _ = tx.send(WorkerEvent::Citations {
                request_id: request_id.clone(),
                session_id: session_id.clone(),
                citations: collected,
            });
        }
    }

    clear_active(&active, &cancel);
    let _ = tx.send(WorkerEvent::RunCompleted {
        request_id,
        session_id,
    });
}

/// Emit a failure event and release the active-run slot.
#[allow(clippy::too_many_arguments)]
fn fail(
    tx: &Sender<WorkerEvent>,
    active: &Arc<Mutex<Option<ActiveRun>>>,
    cancel: &Arc<AtomicBool>,
    request_id: &str,
    session_id: &str,
    error: String,
) {
    clear_active(active, cancel);
    let _ = tx.send(WorkerEvent::RunFailed {
        request_id: request_id.to_owned(),
        session_id: session_id.to_owned(),
        error,
    });
}

/// Release the active-run slot if it still belongs to this run.
fn clear_active(active: &Arc<Mutex<Option<ActiveRun>>>, cancel: &Arc<AtomicBool>) {
    let mut guard = active.lock().unwrap();
    if guard.as_ref().is_some_and(|run| run.matches(cancel)) {
        *guard = None;
    }
}

/// Build the rig agent for a provider, attaching the knowledge tool on
/// native targets. `route` is the resolved OpenCode gateway/dialect for the
/// selected model.
fn build_agent(
    provider: &ProviderSpec,
    api_key: &str,
    model: &str,
    session: &str,
    route: Option<opencode::Route>,
    tool: Option<ProvidedTool>,
) -> anyhow::Result<rig::agent::Agent> {
    // Every provider shares the same builder options: the preamble and the
    // model-call budget. Without the budget, rig allows a single model call
    // and a tool call can never be followed by the model's answer.
    macro_rules! configured {
        ($client:expr, $model:expr) => {
            $client
                .agent($model)
                .preamble(SYSTEM_PREAMBLE)
                .default_max_turns(MAX_MODEL_TURNS)
        };
    }
    let agent = match provider.kind {
        ProviderKind::OpenCode => {
            let route = route.ok_or_else(|| {
                anyhow!("the OpenCode model route was not resolved for '{model}'")
            })?;
            match route.dialect {
                opencode::Dialect::Chat => {
                    let client = opencode::chat_client(route.service, api_key, session)?;
                    finish_agent!(configured!(client, model), tool)
                }
                opencode::Dialect::Responses => {
                    let client = opencode::responses_client(route.service, api_key, session)?;
                    finish_agent!(configured!(client, model), tool)
                }
                opencode::Dialect::Messages => {
                    let client = opencode::messages_client(route.service, api_key, session)?;
                    finish_agent!(configured!(client, model), tool)
                }
            }
        }
        ProviderKind::OpenAi => {
            let client = rig::providers::openai::Client::new(api_key)
                .map_err(|error| anyhow!("failed to build the OpenAI client: {error}"))?;
            finish_agent!(configured!(client, model), tool)
        }
        ProviderKind::ChatGpt => {
            let client = chatgpt::client_from_record(api_key).map_err(anyhow::Error::msg)?;
            finish_agent!(configured!(client, model), tool)
        }
        ProviderKind::Anthropic => {
            let client = rig::providers::anthropic::Client::new(api_key)
                .map_err(|error| anyhow!("failed to build the Anthropic client: {error}"))?;
            finish_agent!(configured!(client, model), tool)
        }
        ProviderKind::DeepSeek => {
            let client = rig::providers::deepseek::Client::new(api_key)
                .map_err(|error| anyhow!("failed to build the DeepSeek client: {error}"))?;
            finish_agent!(configured!(client, model), tool)
        }
    };
    Ok(agent)
}

/// Build the provider/model catalog for the Settings panel from the rig-backed
/// [`providers`] registry. Credential status comes from Worktable's database;
/// OpenCode's model list follows the enabled service checkboxes.
fn build_snapshot(store: &SqliteStore) -> anyhow::Result<ProvidersSnapshot> {
    let credentials = store.list_provider_credentials()?;
    let active_provider = store.get_config("active_provider")?;
    let active_model = store.get_config("active_model")?;
    let services = opencode::Services::from_store(store);

    let providers = providers::PROVIDERS
        .iter()
        .map(|spec| {
            let api_key_set = spec.supports_api_key()
                && credentials
                    .get(spec.id)
                    .is_some_and(|c| c.kind == "api_key" && !c.key.is_empty());
            let oauth_set = spec.supports_oauth()
                && credentials
                    .get(spec.id)
                    .is_some_and(|c| c.kind == "oauth" && !c.key.is_empty());
            let state = if api_key_set || oauth_set {
                crate::model_state::read(store, spec.id)
            } else {
                crate::model_state::ModelState::default()
            };
            ProviderInfo {
                id: spec.id.to_owned(),
                name: spec.name.to_owned(),
                supports_api_key: spec.supports_api_key(),
                supports_oauth: spec.supports_oauth(),
                api_key_set,
                oauth_set,
                groups: if spec.kind == ProviderKind::OpenCode {
                    opencode::SERVICES
                        .iter()
                        .map(|service| ProviderGroup {
                            id: service.id().to_owned(),
                            name: service.name().to_owned(),
                            enabled: services.enabled(*service),
                        })
                        .collect()
                } else {
                    Vec::new()
                },
                models: state.models,
                models_loading: state.loading,
                models_error: state.error,
            }
        })
        .collect();

    Ok(ProvidersSnapshot {
        providers,
        active_provider: active_provider.filter(|value| !value.is_empty()),
        active_model: active_model.filter(|value| !value.is_empty()),
    })
}

fn empty_snapshot() -> ProvidersSnapshot {
    ProvidersSnapshot {
        providers: Vec::new(),
        active_provider: None,
        active_model: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_state;
    use std::time::{Duration, Instant};
    use worktable_events::ModelInfo;

    /// A multi-thread runtime for the worker, matching how the app hosts it.
    fn test_runtime(store: SqliteStore) -> (tokio::runtime::Runtime, AgentRuntime) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let agent = AgentRuntime::start_with_model_loader(
            store,
            runtime.handle().clone(),
            Arc::new(|store, provider| {
                Box::pin(async move {
                    let services = opencode::Services::from_store(&store);
                    let groups = if provider == opencode::ID {
                        opencode::SERVICES
                            .into_iter()
                            .filter(|service| services.enabled(*service))
                            .map(|service| Some(service.name().to_owned()))
                            .collect::<Vec<_>>()
                    } else {
                        vec![None]
                    };
                    Ok(crate::model_catalog::FetchedModels {
                        models: groups
                            .into_iter()
                            .enumerate()
                            .flat_map(|(section, group)| {
                                (0..2).map(move |row| ModelInfo {
                                    id: format!("server-model-{section}-{row}"),
                                    name: format!("Server model {section}-{row}"),
                                    group: group.clone(),
                                    api: None,
                                })
                            })
                            .collect(),
                        warning: None,
                    })
                })
            }),
        );
        (runtime, agent)
    }

    fn temp_store() -> SqliteStore {
        let store = SqliteStore::connect(":memory:").expect("connect in-memory");
        store.migrate().expect("migrate");
        store
    }

    /// Poll the worker event channel until `pred` matches or `timeout` elapses.
    /// Non-matching events are discarded (they are orthogonal traffic).
    fn recv_until(
        agent: &AgentRuntime,
        timeout: Duration,
        pred: impl Fn(&WorkerEvent) -> bool,
    ) -> Option<WorkerEvent> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if let Some(event) = agent.try_recv() {
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
        let (_runtime, agent) = test_runtime(temp_store());
        let event = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(event, WorkerEvent::Ready)
        });
        assert!(event.is_some(), "worker should announce Ready on start");
    }

    #[test]
    fn subscription_agent_builds_without_provider_io() {
        let provider = providers::spec(chatgpt::ID).unwrap();
        build_agent(
            provider,
            r#"{"access_token":"fake-access","account_id":"fake-account"}"#,
            rig::providers::chatgpt::GPT_5_4,
            "session-test",
            None,
            None,
        )
        .expect("subscription Responses agent constructs without I/O");
    }

    #[test]
    fn subscription_login_identity_and_cancellation_guard() {
        let cancel = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let login = ActiveLogin {
            cancel: cancel.clone(),
            notify,
        };
        assert!(login.matches(&cancel));
        assert!(!login.matches(&Arc::new(AtomicBool::new(false))));
        cancel.store(true, Ordering::SeqCst);
        assert!(!login.matches(&cancel));
    }

    #[test]
    fn subscription_cancel_and_logout_invalidate_pending_login_without_io() {
        let store = temp_store();
        let (_runtime, agent) = test_runtime(store.clone());
        let _ = agent.try_recv();
        let cancel = Arc::new(AtomicBool::new(false));
        *agent.login.lock().unwrap() = Some(ActiveLogin {
            cancel: cancel.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
        });
        agent
            .send(WorkerRequest::CancelLogin {
                provider_id: chatgpt::ID.to_owned(),
            })
            .unwrap();
        assert!(cancel.load(Ordering::SeqCst));
        assert!(agent.login.lock().unwrap().is_none());
        assert!(matches!(
            agent.try_recv(),
            Some(WorkerEvent::LoginResult { ok: false, .. })
        ));

        store
            .write_provider_credential(
                chatgpt::ID,
                &ProviderCredential {
                    kind: "oauth".to_owned(),
                    key: r#"{"access_token":"fake-access"}"#.to_owned(),
                },
            )
            .unwrap();
        store.set_config("active_provider", chatgpt::ID).unwrap();
        store.set_config("active_model", "gpt-5.4").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        *agent.login.lock().unwrap() = Some(ActiveLogin {
            cancel: cancel.clone(),
            notify: Arc::new(tokio::sync::Notify::new()),
        });
        agent
            .send(WorkerRequest::Logout {
                provider_id: chatgpt::ID.to_owned(),
            })
            .unwrap();
        assert!(cancel.load(Ordering::SeqCst));
        assert!(agent.login.lock().unwrap().is_none());
        assert!(
            store
                .read_provider_credential(chatgpt::ID)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.get_config("active_provider").unwrap().as_deref(),
            Some("")
        );
        assert_eq!(
            store.get_config("active_model").unwrap().as_deref(),
            Some("")
        );
    }

    #[test]
    fn subscription_cannot_accept_api_keys_or_resolve_api_key_credentials() {
        let store = temp_store();
        let (_runtime, agent) = test_runtime(store.clone());
        let _ = agent.try_recv();
        agent
            .send(WorkerRequest::SetApiKey {
                provider_id: chatgpt::ID.to_owned(),
                api_key: "fake-api-key".to_owned(),
            })
            .unwrap();
        assert!(
            store
                .read_provider_credential(chatgpt::ID)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            agent.try_recv(),
            Some(WorkerEvent::WorkerError { .. })
        ));
        store.set_config("active_provider", chatgpt::ID).unwrap();
        store.set_config("active_model", "gpt-5.4").unwrap();
        store
            .write_provider_credential(
                chatgpt::ID,
                &ProviderCredential {
                    kind: "api_key".to_owned(),
                    key: "fake-api-key".to_owned(),
                },
            )
            .unwrap();
        assert!(matches!(
            resolve_active_provider(&store),
            Err(error) if error == chatgpt::SIGN_IN_AGAIN
        ));
    }

    #[test]
    fn list_providers_returns_the_rig_catalog() {
        let (_runtime, agent) = test_runtime(temp_store());
        let _ = agent.try_recv();

        agent.send(WorkerRequest::ListProviders).unwrap();
        let event = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(event, WorkerEvent::ProvidersSnapshot { .. })
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
            assert!(provider.models.is_empty());
        }
        assert!(
            snapshot.providers.iter().any(|p| p.id == opencode::ID),
            "opencode-go should be in the catalog"
        );
        assert!(
            snapshot.providers.iter().any(|p| p.id == "openai"),
            "openai should be in the catalog"
        );
        assert!(snapshot.active_provider.is_none());
        assert!(snapshot.active_model.is_none());
    }

    #[test]
    fn set_api_key_activates_provider_and_reports_snapshot() {
        let store = temp_store();
        let (_runtime, agent) = test_runtime(store.clone());
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::SetApiKey {
                provider_id: "openai".to_owned(),
                api_key: "sk-test-key".to_owned(),
            })
            .unwrap();

        let event = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(event, WorkerEvent::ProvidersSnapshot { .. })
        });
        let Some(WorkerEvent::ProvidersSnapshot { snapshot }) = event else {
            panic!("expected a ProvidersSnapshot after SetApiKey");
        };
        assert_eq!(snapshot.active_provider.as_deref(), Some("openai"));
        let openai = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == "openai")
            .expect("openai in snapshot");
        assert!(openai.api_key_set, "snapshot should report the stored key");

        let credential = store
            .read_provider_credential("openai")
            .unwrap()
            .expect("credential stored");
        assert_eq!(credential.key, "sk-test-key");
    }

    #[test]
    fn set_api_key_activates_provider_and_selects_a_model() {
        let store = temp_store();
        let (_runtime, initial_agent) = test_runtime(store.clone());
        drop(initial_agent);
        let (release, result) = tokio::sync::oneshot::channel();
        let result = Arc::new(Mutex::new(Some(result)));
        let agent = AgentRuntime::start_with_model_loader(
            store,
            _runtime.handle().clone(),
            Arc::new(move |_, _| {
                let result = result.lock().unwrap().take().unwrap();
                Box::pin(async move { result.await.unwrap() })
            }),
        );
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::SetApiKey {
                provider_id: opencode::ID.to_owned(),
                api_key: "sk-test".to_owned(),
            })
            .unwrap();

        let event = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(event, WorkerEvent::ConfigChanged { .. })
        });
        let Some(WorkerEvent::ConfigChanged {
            active_provider,
            active_model,
        }) = event
        else {
            panic!("expected ConfigChanged after SetApiKey");
        };
        assert_eq!(active_provider.as_deref(), Some(opencode::ID));
        assert!(
            active_model.is_none(),
            "saving a key is not model discovery"
        );
        release
            .send(Ok(crate::model_catalog::FetchedModels {
                models: vec![ModelInfo {
                    id: "server-model-0-0".to_owned(),
                    name: "Server model 0-0".to_owned(),
                    group: Some(opencode::Service::Go.name().to_owned()),
                    api: None,
                }],
                warning: None,
            }))
            .unwrap();
        let event = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(event, WorkerEvent::ConfigChanged { active_provider, active_model }
                if active_provider.as_deref() == Some(opencode::ID)
                    && active_model.as_deref() == Some("server-model-0-0"))
        });
        assert!(
            event.is_some(),
            "successful discovery selects its first row"
        );
    }

    #[test]
    fn saving_another_key_keeps_the_provider_model_pair_consistent() {
        let store = temp_store();
        store.set_config("active_provider", "openai").unwrap();
        store.set_config("active_model", "gpt-5-mini").unwrap();
        let (_runtime, agent) = test_runtime(store.clone());

        agent.set_api_key("deepseek", "test-key-no-network");
        let _ = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(event, WorkerEvent::ProvidersSnapshot { snapshot }
                if snapshot.active_model.as_deref() == Some("server-model-0-0"))
        });
        let snapshot = build_snapshot(&store).unwrap();
        assert_eq!(snapshot.active_provider.as_deref(), Some("deepseek"));
        assert_eq!(snapshot.active_model.as_deref(), Some("server-model-0-0"));

        // An empty enabled catalog must not retain a different provider's id.
        opencode::Services {
            go: false,
            zen: false,
        }
        .write_to_store(&store)
        .unwrap();
        agent.set_api_key(opencode::ID, "test-key-no-network");
        let snapshot = build_snapshot(&store).unwrap();
        assert_eq!(snapshot.active_provider.as_deref(), Some(opencode::ID));
        assert!(snapshot.active_model.is_none());
    }

    #[test]
    fn provider_groups_gate_the_catalog_and_revalidate_the_model() {
        let store = temp_store();
        store.set_config("unrelated_setting", "preserved").unwrap();
        let (_runtime, agent) = test_runtime(store.clone());
        agent
            .send(WorkerRequest::SetApiKey {
                provider_id: opencode::ID.to_owned(),
                api_key: "sk-test".to_owned(),
            })
            .unwrap();
        assert!(
            recv_until(&agent, Duration::from_secs(2), |event| {
                matches!(event, WorkerEvent::ProvidersSnapshot { snapshot }
                    if snapshot.active_model.as_deref() == Some("server-model-0-0"))
            })
            .is_some()
        );
        agent
            .send(WorkerRequest::SetModel {
                provider_id: opencode::ID.to_owned(),
                model_id: "server-model-0-1".to_owned(),
            })
            .unwrap();
        agent
            .send(WorkerRequest::SetProviderGroup {
                provider_id: opencode::ID.to_owned(),
                group_id: "go".to_owned(),
                enabled: false,
            })
            .unwrap();

        assert!(
            recv_until(&agent, Duration::from_secs(2), |event| {
                matches!(event, WorkerEvent::ProvidersSnapshot { snapshot }
                    if snapshot.providers.iter().any(|provider|
                        provider.id == opencode::ID && !provider.models_loading
                            && provider.models.len() == 2
                            && provider.models.iter().all(|model|
                                model.group.as_deref() == Some(opencode::Service::Zen.name()))))
            })
            .is_some()
        );
        let snapshot = build_snapshot(&store).unwrap();
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == opencode::ID)
            .unwrap();
        assert!(
            !provider
                .groups
                .iter()
                .find(|group| group.id == "go")
                .unwrap()
                .enabled
        );
        assert!(
            provider
                .groups
                .iter()
                .find(|group| group.id == "zen")
                .unwrap()
                .enabled
        );
        assert!(
            !provider
                .models
                .iter()
                .any(|model| model.group.as_deref() == Some(opencode::Service::Go.name()))
        );
        assert!(
            provider
                .models
                .iter()
                .any(|model| model.id == "server-model-0-0")
        );
        assert_eq!(snapshot.active_model.as_deref(), Some("server-model-0-0"));

        // A stale picker cannot select a model after its service is disabled.
        agent
            .send(WorkerRequest::SetModel {
                provider_id: opencode::ID.to_owned(),
                model_id: "server-model-1-1".to_owned(),
            })
            .unwrap();
        assert_eq!(
            store.get_config("active_model").unwrap().as_deref(),
            Some("server-model-0-0")
        );
        assert!(
            recv_until(&agent, Duration::from_secs(2), |event| {
                matches!(event, WorkerEvent::WorkerError { .. })
            })
            .is_some()
        );

        agent
            .send(WorkerRequest::SetProviderGroup {
                provider_id: opencode::ID.to_owned(),
                group_id: "zen".to_owned(),
                enabled: false,
            })
            .unwrap();
        let snapshot = build_snapshot(&store).unwrap();
        assert!(snapshot.active_provider.is_none());
        assert!(snapshot.active_model.is_none());
        let provider = snapshot
            .providers
            .iter()
            .find(|p| p.id == opencode::ID)
            .unwrap();
        assert!(provider.models.is_empty());
        assert!(provider.groups.iter().all(|group| !group.enabled));
        assert!(provider.api_key_set, "disabling services must not log out");
        assert_eq!(
            store
                .read_provider_credential(opencode::ID)
                .unwrap()
                .unwrap()
                .key,
            "sk-test"
        );
        assert_eq!(
            store.get_config("unrelated_setting").unwrap().as_deref(),
            Some("preserved")
        );
    }

    #[test]
    fn opencode_agents_build_for_each_gateway_and_dialect_without_io() {
        let provider = providers::spec(opencode::ID).unwrap();
        for service in opencode::SERVICES {
            let services = opencode::Services {
                go: service == opencode::Service::Go,
                zen: service == opencode::Service::Zen,
            };
            for api in ["chat", "responses", "messages"] {
                let model = ModelInfo {
                    id: "server-model".to_owned(),
                    name: "Server model".to_owned(),
                    group: Some(service.name().to_owned()),
                    api: Some(api.to_owned()),
                };
                let route = opencode::route(&model, services).unwrap();
                assert_eq!(route.service, service);
                build_agent(
                    provider,
                    "sk-test",
                    &model.id,
                    "session-test",
                    Some(route),
                    None,
                )
                .expect("each offered model must have a constructible client");
            }
        }
    }

    #[test]
    fn snapshot_grouping_matches_the_dispatch_route_for_every_enabled_set() {
        let store = temp_store();
        for services in [
            opencode::Services::default(),
            opencode::Services {
                go: true,
                zen: false,
            },
            opencode::Services {
                go: false,
                zen: true,
            },
            opencode::Services {
                go: false,
                zen: false,
            },
        ] {
            services.write_to_store(&store).unwrap();
            let snapshot = build_snapshot(&store).unwrap();
            for provider in snapshot.providers {
                if provider.id == opencode::ID {
                    assert!(
                        provider.models.is_empty(),
                        "unconfigured providers advertise no models"
                    );
                    for model in provider.models {
                        let route = opencode::route(&model, services).unwrap();
                        assert_eq!(model.group.as_deref(), Some(route.service.name()));
                        assert!(services.enabled(route.service));
                    }
                } else {
                    assert!(provider.groups.is_empty());
                    assert!(provider.models.iter().all(|model| model.group.is_none()));
                }
            }
        }
    }

    #[test]
    fn legacy_opencode_selection_and_credential_resolve_without_migration() {
        let store = temp_store();
        store
            .write_provider_credential(
                "opencode-go",
                &ProviderCredential {
                    kind: "api_key".to_owned(),
                    key: "sk-test-legacy".to_owned(),
                },
            )
            .unwrap();
        store.set_config("active_provider", "opencode-go").unwrap();
        store
            .set_config("active_model", "legacy-server-model")
            .unwrap();
        model_state::write(
            &store,
            "opencode-go",
            &model_state::ModelState {
                models: vec![ModelInfo {
                    id: "legacy-server-model".to_owned(),
                    name: "Legacy server model".to_owned(),
                    group: Some(opencode::Service::Go.name().to_owned()),
                    api: Some("chat".to_owned()),
                }],
                ..Default::default()
            },
        )
        .unwrap();
        let config = resolve_active_provider(&store).unwrap();
        assert_eq!(config.key, "sk-test-legacy");
        assert_eq!(config.model, "legacy-server-model");
        assert_eq!(
            config.route,
            Some(opencode::Route {
                service: opencode::Service::Go,
                dialect: opencode::Dialect::Chat,
            })
        );
        opencode::Services {
            go: false,
            zen: true,
        }
        .write_to_store(&store)
        .unwrap();
        assert!(resolve_active_provider(&store).is_err());
    }

    #[test]
    fn set_model_persists_selection() {
        let store = temp_store();
        let (_runtime, agent) = test_runtime(store.clone());
        let _ = agent.try_recv();

        agent.set_api_key("openai", "test-key-no-network");
        assert!(
            recv_until(&agent, Duration::from_secs(2), |event| {
                matches!(event, WorkerEvent::ProvidersSnapshot { snapshot }
                    if snapshot.active_model.as_deref() == Some("server-model-0-0"))
            })
            .is_some()
        );
        agent
            .send(WorkerRequest::SetModel {
                provider_id: "openai".to_owned(),
                model_id: "server-model-0-1".to_owned(),
            })
            .unwrap();

        let event = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(event, WorkerEvent::ConfigChanged { .. })
        });
        let Some(WorkerEvent::ConfigChanged {
            active_provider,
            active_model,
        }) = event
        else {
            panic!("expected ConfigChanged after SetModel");
        };
        assert_eq!(active_provider.as_deref(), Some("openai"));
        assert_eq!(active_model.as_deref(), Some("server-model-0-1"));
        assert_eq!(
            store.get_config("active_model").unwrap().as_deref(),
            Some("server-model-0-1")
        );
    }

    #[test]
    fn logout_clears_credential_and_active_selection() {
        let store = temp_store();
        let (_runtime, agent) = test_runtime(store.clone());
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::SetApiKey {
                provider_id: "openai".to_owned(),
                api_key: "sk-test-key".to_owned(),
            })
            .unwrap();
        let _ = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(event, WorkerEvent::ProvidersSnapshot { .. })
        });

        agent
            .send(WorkerRequest::Logout {
                provider_id: "openai".to_owned(),
            })
            .unwrap();
        let event = recv_until(&agent, Duration::from_secs(2), |event| {
            matches!(
                event,
                WorkerEvent::ConfigChanged {
                    active_provider: None,
                    active_model: None,
                }
            )
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
        let (_runtime, agent) = test_runtime(temp_store());
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::Prompt {
                request_id: "req-1".to_owned(),
                session_id: "sess-1".to_owned(),
                content: "hello".to_owned(),
            })
            .unwrap();

        let event = recv_until(&agent, Duration::from_secs(5), |event| {
            matches!(event, WorkerEvent::RunFailed { .. })
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

    #[test]
    fn prompt_with_unknown_provider_fails_instead_of_wedging() {
        let store = temp_store();
        store
            .set_config("active_provider", "provider-that-left-the-build")
            .unwrap();
        store.set_config("active_model", "some-model").unwrap();
        store
            .write_provider_credential(
                "provider-that-left-the-build",
                &ProviderCredential {
                    kind: "api_key".to_owned(),
                    key: "sk-test".to_owned(),
                },
            )
            .unwrap();

        let (_runtime, agent) = test_runtime(store);
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::Prompt {
                request_id: "req-unknown".to_owned(),
                session_id: "sess-1".to_owned(),
                content: "hello".to_owned(),
            })
            .unwrap();

        let event = recv_until(&agent, Duration::from_secs(5), |event| {
            matches!(event, WorkerEvent::RunFailed { .. })
        });
        let Some(WorkerEvent::RunFailed { error, .. }) = event else {
            panic!("prompt with an unknown provider must RunFailed, not wedge");
        };
        assert!(
            error.contains("not in this build's catalog"),
            "the error should name the provider problem: {error}"
        );
    }

    #[test]
    fn prompt_without_api_key_explains_the_setup_step() {
        let store = temp_store();
        store.set_config("active_provider", "openai").unwrap();
        store.set_config("active_model", "gpt-5.5").unwrap();

        let (_runtime, agent) = test_runtime(store);
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::Prompt {
                request_id: "req-no-key".to_owned(),
                session_id: "sess-1".to_owned(),
                content: "hello".to_owned(),
            })
            .unwrap();

        let event = recv_until(&agent, Duration::from_secs(5), |event| {
            matches!(event, WorkerEvent::RunFailed { .. })
        });
        let Some(WorkerEvent::RunFailed { error, .. }) = event else {
            panic!("a keyless provider must fail with setup guidance");
        };
        assert!(
            error.contains("no API key is stored"),
            "unexpected error: {error}"
        );
    }

    /// The loop that broke in the field: the model calls `search_knowledge`,
    /// rig executes the tool, and the run must reach a final answer instead of
    /// burning rig's implicit one-model-call budget. The mock model scripts a
    /// tool turn then an answer turn; no provider is contacted.
    #[test]
    fn agent_tool_call_is_followed_by_a_final_answer() {
        use rig::agent::AgentBuilder;
        use rig::streaming::{StreamedUserContent, StreamingPrompt as _};
        use rig::test_utils::{MockCompletionModel, MockStreamEvent};

        let dir = std::env::temp_dir().join(format!("wt-agent-loop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let graph_path = dir.join("helix.json");
        let helix = worktable_helix::HelixClient::open_embedded(graph_path.clone());
        let seeded = worktable_db::Entry {
            id: "sunset-1".to_owned(),
            content: "The sunset over the mountains was breathtaking".to_owned(),
            title: None,
            source: "Worktable".to_owned(),
            created_at: 1_000,
        };

        let model = MockCompletionModel::from_stream_turns([
            vec![
                MockStreamEvent::tool_call(
                    "call_1",
                    "search_knowledge",
                    serde_json::json!({"query": "sunset"}),
                ),
                // Each scripted turn needs its terminal record.
                MockStreamEvent::final_response_with_total_tokens(7),
            ],
            vec![
                MockStreamEvent::text("Your note says the sunset was breathtaking [1]."),
                MockStreamEvent::final_response_with_total_tokens(9),
            ],
        ]);

        let collector = crate::helix_tool::CitationCollector::new();
        let tool = crate::helix_tool::SearchKnowledgeTool::with_url(Some(
            graph_path.to_string_lossy().into_owned(),
        ))
        .with_citations(collector.clone());
        let agent = AgentBuilder::new(model.clone())
            .preamble(SYSTEM_PREAMBLE)
            .default_max_turns(MAX_MODEL_TURNS)
            .tool(tool)
            .build();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime
            .block_on(helix.sync_entry(&seeded))
            .expect("seed graph");

        let (saw_tool_call, saw_tool_result, saw_final, answer) = runtime.block_on(async {
            let mut stream = agent.stream_prompt("What did I save about sunsets?").await;
            let (mut tool_call, mut tool_result, mut final_response) = (false, false, false);
            let mut answer = String::new();
            while let Some(item) = stream.next().await {
                match item.expect("stream item") {
                    MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::ToolCall { .. },
                    ) => tool_call = true,
                    MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                        ..
                    }) => tool_result = true,
                    MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                        text,
                    )) => answer.push_str(&text.text),
                    MultiTurnStreamItem::FinalResponse(_) => final_response = true,
                    _ => {}
                }
            }
            (tool_call, tool_result, final_response, answer)
        });

        assert!(saw_tool_call, "the scripted tool call should surface");
        assert!(
            saw_tool_result,
            "the tool should execute and report its result"
        );
        assert!(
            saw_final,
            "the run must reach a final response after the tool call"
        );
        assert!(
            answer.contains("[1]"),
            "the answer should follow the tool output: {answer}"
        );
        assert!(
            model.request_count() >= 2,
            "a tool call plus an answer needs at least two model calls, got {}",
            model.request_count()
        );
        let citations = collector.take();
        assert_eq!(citations.len(), 1, "the search hit becomes a citation");
        assert_eq!(citations[0].entry_id, "sunset-1");
    }

    #[test]
    fn history_replay_drops_reasoning_parts() {
        let history = vec![
            Message::user("hello"),
            Message::Assistant {
                id: None,
                content: vec![
                    AssistantContent::Reasoning(rig::message::Reasoning::new(
                        "internal chain of thought",
                    )),
                    AssistantContent::Text(rig::message::Text::new("hi")),
                ],
            },
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::Reasoning(rig::message::Reasoning::new(
                    "only reasoning",
                ))],
            },
        ];
        let sanitized = sanitize_history(history);
        assert_eq!(sanitized.len(), 2, "reasoning-only messages are dropped");
        match &sanitized[1] {
            Message::Assistant { content, .. } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(content[0], AssistantContent::Text(_)));
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn topic_json_tolerates_fences_and_string_lists() {
        let parsed = parse_topic_json(
            "Sure!\n```json\n[{\"id\":\"a\",\"topics\":[\"Sunset\",\" mountains \"]},\
             {\"id\":\"b\",\"topics\":\"routing, sessions\"}]\n```",
        );
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "a");
        assert_eq!(parsed[0].1, vec!["Sunset", " mountains "]);
        assert_eq!(
            parsed[1],
            (
                "b".to_owned(),
                vec!["routing".to_owned(), "sessions".to_owned()]
            )
        );
        assert!(parse_topic_json("no json here").is_empty());
        assert!(parse_topic_json("[{\"id\":\"a\",\"topics\":[]}]").is_empty());
    }

    /// The enrichment pass asks the model once and returns `(id, topics)`
    /// pairs for the graph, with no provider contacted (mock model).
    #[test]
    fn enrich_with_agent_returns_topics_for_entry_ids() {
        use rig::agent::AgentBuilder;
        use rig::test_utils::{MockCompletionModel, MockTurn};

        let model = MockCompletionModel::new([MockTurn::text(
            r#"[{"id":"e1","topics":["sunset","mountains"]},{"id":"e2","topics":["routing"]}]"#,
        )]);
        let agent = AgentBuilder::new(model.clone())
            .preamble(KNOWLEDGE_PREAMBLE)
            .default_max_turns(1)
            .build();
        let entries = vec![
            worktable_db::Entry {
                id: "e1".to_owned(),
                content: "The sunset over the mountains".to_owned(),
                title: None,
                source: "Worktable".to_owned(),
                created_at: 1_000,
            },
            worktable_db::Entry {
                id: "e2".to_owned(),
                content: "Routing notes for the sessions work".to_owned(),
                title: None,
                source: "Worktable".to_owned(),
                created_at: 900,
            },
        ];

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let topics = runtime
            .block_on(enrich_with_agent(&agent, &entries))
            .expect("enrichment succeeds");
        assert_eq!(
            topics,
            vec![
                (
                    "e1".to_owned(),
                    vec!["sunset".to_owned(), "mountains".to_owned()]
                ),
                ("e2".to_owned(), vec!["routing".to_owned()]),
            ]
        );
        let requests = model.requests();
        assert_eq!(requests.len(), 1, "one model call per batch");
    }

    #[test]
    fn enrich_topics_without_a_provider_fails_with_setup_guidance() {
        let store = temp_store();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let error = runtime
            .block_on(enrich_topics(
                &store,
                vec![worktable_db::Entry {
                    id: "e1".to_owned(),
                    content: "note".to_owned(),
                    title: None,
                    source: "Worktable".to_owned(),
                    created_at: 1,
                }],
            ))
            .expect_err("no provider must fail");
        assert!(
            error.to_string().contains("no AI provider configured"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn cancel_without_active_run_is_quiet() {
        let (_runtime, agent) = test_runtime(temp_store());
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::Cancel {
                request_id: "ghost".to_owned(),
                session_id: String::new(),
            })
            .unwrap();

        let event = recv_until(&agent, Duration::from_millis(200), |event| {
            matches!(event, WorkerEvent::RunFailed { .. })
        });
        assert!(
            event.is_none(),
            "cancel with no active run must not fabricate a failure"
        );
    }

    #[test]
    fn saved_context_restores_only_the_selected_chat_without_a_provider() {
        let store = temp_store();
        let first = vec![
            Message::user("Remember: my project is called Atlas"),
            Message::assistant("I'll remember Atlas."),
        ];
        let second = vec![Message::user("This is a separate chat")];
        store
            .save_session_history("a", &serde_json::to_string(&first).unwrap())
            .unwrap();
        store
            .save_session_history("b", &serde_json::to_string(&second).unwrap())
            .unwrap();
        let empty_memory = HashMap::new();
        let restored = session_history(&store, &empty_memory, "a").unwrap();
        assert_eq!(
            serde_json::to_value(restored).unwrap(),
            serde_json::to_value(&first).unwrap()
        );
        assert_eq!(
            session_history(&store, &empty_memory, "b").unwrap().len(),
            1
        );
        assert!(
            session_history(&store, &empty_memory, "new")
                .unwrap()
                .is_empty()
        );
        let mut memory = HashMap::new();
        memory.insert("a".into(), vec![Message::user("Newer in-memory context")]);
        assert_eq!(session_history(&store, &memory, "a").unwrap().len(), 1);
        store.save_session_history("corrupt", "{}").unwrap();
        assert!(session_history(&store, &empty_memory, "corrupt").is_err());
    }

    #[test]
    fn cancel_is_synchronous_and_marks_the_active_run() {
        let store = temp_store();
        store.set_config("active_provider", "openai").unwrap();
        store.set_config("active_model", "gpt-5.5").unwrap();
        let (_runtime, agent) = test_runtime(store);
        let _ = agent.try_recv();

        // Simulate an in-flight run without touching the network.
        let cancel = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        *agent.active.lock().unwrap() = Some(ActiveRun {
            request_id: "req-busy".to_owned(),
            session_id: "sess".to_owned(),
            cancel: Arc::clone(&cancel),
            notify: Arc::clone(&notify),
        });

        agent
            .send(WorkerRequest::Cancel {
                request_id: "req-busy".to_owned(),
                session_id: "sess".to_owned(),
            })
            .unwrap();

        assert!(cancel.load(Ordering::SeqCst), "the run must be flagged");
        // Cancellation must also wake the stream so a stalled provider
        // response is dropped immediately.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_millis(200), notify.notified())
                .await
                .expect("cancel should wake the run immediately");
        });
        let event = recv_until(
            &agent,
            Duration::from_millis(200),
            |event| matches!(event, WorkerEvent::RunFailed { error, .. } if error == ABORTED_BY_USER),
        );
        assert!(event.is_some(), "cancel should report the abort");
        assert!(agent.active.lock().unwrap().is_none(), "slot released");
    }
}
