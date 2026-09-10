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
//! OpenCode Go client is in [`crate::opencode_go`].

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
use worktable_events::{ModelInfo, ProviderInfo, ProvidersSnapshot};

use crate::{
    opencode_go,
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
    let key = store
        .read_provider_credential(&provider_id)
        .ok()
        .flatten()
        .map(|credential| credential.key);
    let Some(key) = key.filter(|key| !key.trim().is_empty()) else {
        return Err(format!(
            "no API key is stored for {provider_id}; add one in Settings"
        ));
    };
    Ok(ActiveProviderConfig {
        spec,
        key,
        model,
        session: uuid::Uuid::new_v4().to_string(),
    })
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

/// Extract topics for `entries` with the active provider's model. Each batch
/// is one model call; the graph is updated by the caller.
pub(crate) async fn enrich_topics(
    store: &SqliteStore,
    entries: Vec<worktable_db::Entry>,
) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let config = resolve_active_provider(store).map_err(anyhow::Error::msg)?;
    let agent = build_agent(
        config.spec,
        &config.key,
        &config.model,
        &config.session,
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
    /// Conversation transcript per session, seeded by the previous run's
    /// `PromptResponse::messages`.
    history: Arc<Mutex<HashMap<String, Vec<Message>>>>,
    /// Stable id for the OpenCode Go `x-opencode-session` header when a
    /// request arrives without a session id.
    default_session: String,
}

impl AgentRuntime {
    /// Create the runtime on `tokio`'s executor. A `Ready` event is emitted
    /// once so the event pump reports the AI worker as ready.
    pub fn start(store: SqliteStore, tokio: tokio::runtime::Handle) -> Self {
        let (events_tx, events_rx) = channel();
        let runtime = Self {
            store,
            tokio,
            events_tx,
            events_rx: Mutex::new(events_rx),
            active: Arc::new(Mutex::new(None)),
            history: Arc::new(Mutex::new(HashMap::new())),
            default_session: uuid::Uuid::new_v4().to_string(),
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
            WorkerRequest::ListProviders => self.emit_snapshot(),
            WorkerRequest::SetApiKey {
                provider_id,
                api_key,
            } => self.set_api_key(&provider_id, &api_key),
            WorkerRequest::SetModel {
                provider_id,
                model_id,
            } => {
                if self
                    .store
                    .set_config("active_provider", &provider_id)
                    .is_ok()
                {
                    let _ = self.store.set_config("active_model", &model_id);
                }
                self.emit_config_changed();
            }
            WorkerRequest::Logout { provider_id } => self.logout(&provider_id),
            WorkerRequest::LoginOAuth { provider_id } => {
                let _ = self.events_tx.send(WorkerEvent::LoginResult {
                    provider_id,
                    ok: false,
                    error: Some(
                        "OAuth login is not available; configure an API key instead".to_owned(),
                    ),
                });
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
        let credential = ProviderCredential {
            kind: "api_key".to_owned(),
            key: api_key.to_owned(),
        };
        if let Err(error) = self
            .store
            .write_provider_credential(provider_id, &credential)
        {
            let _ = self.events_tx.send(WorkerEvent::WorkerError {
                error: format!("failed to store API key: {error}"),
            });
            return;
        }

        // Activate the provider if nothing is selected yet.
        let active_provider = self.store.get_config("active_provider").ok().flatten();
        if active_provider.as_deref().is_none_or(str::is_empty) {
            let _ = self.store.set_config("active_provider", provider_id);
        }

        // A provider without a model can never serve a prompt, and picking a
        // model is an easy-to-miss separate step — default to the catalog's
        // first model unless a valid one is already selected.
        let active_model = self.store.get_config("active_model").ok().flatten();
        let model_needed = match active_model.as_deref() {
            None | Some("") => true,
            Some(model) => !providers::model_known(provider_id, model),
        };
        if model_needed && let Some(first) = providers::first_model(provider_id) {
            let _ = self.store.set_config("active_model", first);
        }

        self.emit_config_changed();
    }

    fn logout(&self, provider_id: &str) {
        let _ = self.store.delete_provider_credential(provider_id);
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
    let config = match resolve_active_provider(&store) {
        Ok(config) => config,
        Err(error) => {
            fail(&tx, &active, &cancel, &request_id, &session_id, error);
            return;
        }
    };

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
    let prior: Vec<Message> = sanitize_history(
        history
            .lock()
            .unwrap()
            .get(&session_id)
            .cloned()
            .unwrap_or_default(),
    );
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
/// native targets.
fn build_agent(
    provider: &ProviderSpec,
    api_key: &str,
    model: &str,
    session: &str,
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
        ProviderKind::OpenCodeGo => {
            let client = opencode_go::client(api_key, session)?;
            finish_agent!(configured!(client, model), tool)
        }
        ProviderKind::OpenAi => {
            let client = rig::providers::openai::Client::new(api_key)
                .map_err(|error| anyhow!("failed to build the OpenAI client: {error}"))?;
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
/// [`providers`] registry. Credential status comes from Worktable's database.
fn build_snapshot(store: &SqliteStore) -> anyhow::Result<ProvidersSnapshot> {
    let credentials = store.list_provider_credentials()?;
    let active_provider = store.get_config("active_provider")?;
    let active_model = store.get_config("active_model")?;

    let providers = providers::PROVIDERS
        .iter()
        .map(|spec| ProviderInfo {
            id: spec.id.to_owned(),
            name: spec.name.to_owned(),
            supports_api_key: true,
            supports_oauth: false,
            api_key_set: credentials.contains_key(spec.id),
            oauth_set: false,
            models: spec
                .models
                .iter()
                .map(|(id, name)| ModelInfo {
                    id: (*id).to_owned(),
                    name: (*name).to_owned(),
                })
                .collect(),
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
    use std::time::{Duration, Instant};

    /// A multi-thread runtime for the worker, matching how the app hosts it.
    fn test_runtime(store: SqliteStore) -> (tokio::runtime::Runtime, AgentRuntime) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let agent = AgentRuntime::start(store, runtime.handle().clone());
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
            assert!(!provider.models.is_empty());
        }
        assert!(
            snapshot.providers.iter().any(|p| p.id == opencode_go::ID),
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
        let (_runtime, agent) = test_runtime(store);
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::SetApiKey {
                provider_id: opencode_go::ID.to_owned(),
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
        assert_eq!(active_provider.as_deref(), Some(opencode_go::ID));
        assert_eq!(
            active_model.as_deref(),
            Some("glm-5.3"),
            "the catalog's first model should be auto-selected"
        );
    }

    #[test]
    fn set_model_persists_selection() {
        let store = temp_store();
        let (_runtime, agent) = test_runtime(store.clone());
        let _ = agent.try_recv();

        agent
            .send(WorkerRequest::SetModel {
                provider_id: "openai".to_owned(),
                model_id: "gpt-5.5".to_owned(),
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
        assert_eq!(active_model.as_deref(), Some("gpt-5.5"));
        assert_eq!(
            store.get_config("active_model").unwrap().as_deref(),
            Some("gpt-5.5")
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
            matches!(event, WorkerEvent::ConfigChanged { .. })
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
