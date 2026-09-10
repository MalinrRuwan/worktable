use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use tokio::{
    sync::Mutex,
    task::JoinHandle,
    time::{self, Duration},
};
use uuid::Uuid;
use worktable_db::SqliteStore;
use worktable_events::{AuthNotifyKind, AuthPromptKind, EventBus, WorktableEvent};

use crate::{
    agent_runtime::AgentRuntime,
    worker_protocol::{WorkerEvent, WorkerRequest},
};

const SESSION_LEASE_MS: i64 = 30_000;
const EVENT_PUMP_POLL_MS: u64 = 8;

#[derive(Debug, Clone)]
pub struct AiRun {
    pub run_id: String,
    pub request_id: String,
    pub session_id: String,
}

#[derive(Clone)]
pub struct WorktableRuntime {
    store: SqliteStore,
    ai_agent: Arc<Mutex<Option<AgentRuntime>>>,
    owner_id: String,
    events: EventBus,
    lease_tasks: Arc<Mutex<BTreeMap<String, JoinHandle<()>>>>,
    active_runs: Arc<Mutex<BTreeMap<String, AiRun>>>,
    pump_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl WorktableRuntime {
    pub async fn connect(database_path: &str) -> anyhow::Result<Self> {
        let store = SqliteStore::connect(database_path)?;
        store.migrate()?;

        Ok(Self {
            store,
            ai_agent: Arc::new(Mutex::new(None)),
            owner_id: Uuid::new_v4().to_string(),
            events: EventBus::new(256),
            lease_tasks: Arc::new(Mutex::new(BTreeMap::new())),
            active_runs: Arc::new(Mutex::new(BTreeMap::new())),
            pump_task: Arc::new(Mutex::new(None)),
        })
    }

    pub fn events(&self) -> EventBus {
        self.events.clone()
    }

    pub fn database_path(&self) -> &str {
        self.store.database_path()
    }

    pub async fn list_entries(&self, limit: usize) -> anyhow::Result<Vec<worktable_db::Entry>> {
        self.store.list_entries(limit)
    }

    pub async fn insert_entry(&self, entry: &worktable_db::Entry) -> anyhow::Result<()> {
        self.store.insert_entry(entry)?;

        // Best-effort Helix mirror. SQLite remains source-of-truth; if Helix is
        // not running we log and continue. Never fail the SQLite insert.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let entry_owned = entry.clone();
            spawn_helix_mirror(self.store.database_path(), move |helix| {
                if let Err(err) = helix.sync_entry_blocking(&entry_owned) {
                    eprintln!("[worktable] Helix sync_entry failed (fallback to SQLite): {err}");
                }
            });
        }

        Ok(())
    }

    /// Replace an entry's content and refresh its knowledge-graph node.
    pub async fn update_entry(&self, entry: &worktable_db::Entry) -> anyhow::Result<()> {
        self.store.update_entry_content(&entry.id, &entry.content)?;

        #[cfg(not(target_arch = "wasm32"))]
        {
            let entry_owned = entry.clone();
            spawn_helix_mirror(self.store.database_path(), move |helix| {
                if let Err(err) = helix.sync_entry_blocking(&entry_owned) {
                    eprintln!("[worktable] Helix sync_entry failed (fallback to SQLite): {err}");
                }
            });
        }

        Ok(())
    }

    pub async fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
        self.store.delete_entry(id)?;

        #[cfg(not(target_arch = "wasm32"))]
        {
            let id_owned = id.to_owned();
            spawn_helix_mirror(self.store.database_path(), move |helix| {
                if let Err(err) = helix.delete_entry_blocking(&id_owned) {
                    eprintln!("[worktable] Helix delete_entry failed (fallback to SQLite): {err}");
                }
            });
        }

        Ok(())
    }

    /// Helix best-effort search (for UI or direct API callers). On WASM or
    /// when Helix is down returns `Ok(empty)`.
    pub async fn search_helix(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let helix = worktable_helix::HelixClient::from_env();
            helix.search_best_effort(query, limit).await
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (query, limit);
            Ok(Vec::new())
        }
    }

    pub async fn start_ai_worker(&self) -> anyhow::Result<()> {
        let mut agent = self.ai_agent.lock().await;
        if agent.is_some() {
            return Err(anyhow::anyhow!("AI worker is already running"));
        }

        // Runs from a previous process lifetime can never finish — close them
        // out so `wt_ai_runs` never accumulates permanent `running` rows and
        // the session state machine starts clean.
        let _ = self.store.fail_stale_runs(unix_time_ms()?);

        let worker = AgentRuntime::start(self.store.clone(), tokio::runtime::Handle::current());
        *agent = Some(worker);

        // Start the event pump that drains worker output into the event bus and
        // finalizes runs. This is what keeps sessions/leases from being leaked.
        let runtime = self.clone();
        let pump = tokio::spawn(async move {
            runtime.pump_worker_events().await;
        });
        *self.pump_task.lock().await = Some(pump);

        Ok(())
    }

    /// Extract AI topics for knowledge-graph entries with the active
    /// provider. Background maintenance, so it skips session leases.
    pub async fn enrich_topics(
        &self,
        entries: Vec<worktable_db::Entry>,
    ) -> anyhow::Result<Vec<(String, Vec<String>)>> {
        crate::agent_runtime::enrich_topics(&self.store, entries).await
    }

    pub async fn submit_prompt(
        &self,
        request_id: &str,
        session_id: &str,
        content: &str,
    ) -> anyhow::Result<Option<AiRun>> {
        let now_ms = unix_time_ms()?;
        self.store.ensure_session(session_id, None, now_ms)?;

        let run_id = Uuid::new_v4().to_string();
        let claimed = self.store.claim_session_lease(
            session_id,
            &self.owner_id,
            &run_id,
            now_ms,
            now_ms + SESSION_LEASE_MS,
        )?;
        if !claimed {
            return Ok(None);
        }

        let started = self
            .store
            .begin_run(&run_id, session_id, request_id, now_ms)?;
        if !started {
            self.store
                .release_session_lease(session_id, &self.owner_id, &run_id)?;
            return Ok(None);
        }

        let send_result = {
            let agent = self.ai_agent.lock().await;
            match agent.as_ref() {
                Some(agent) => agent.send(WorkerRequest::Prompt {
                    request_id: request_id.to_owned(),
                    session_id: session_id.to_owned(),
                    content: content.to_owned(),
                }),
                None => Err(anyhow::anyhow!("AI worker has not been started")),
            }
        };

        if let Err(error) = send_result {
            let finish_error = self.store.finish_run(
                &run_id,
                session_id,
                "failed",
                Some(&error.to_string()),
                unix_time_ms()?,
            );
            self.store
                .release_session_lease(session_id, &self.owner_id, &run_id)?;
            finish_error?;
            return Err(error);
        }

        let run = AiRun {
            run_id,
            request_id: request_id.to_owned(),
            session_id: session_id.to_owned(),
        };
        self.active_runs
            .lock()
            .await
            .insert(run.request_id.clone(), run.clone());
        self.start_lease_renewal(&run).await;

        self.events.publish(WorktableEvent::AiRunStarted {
            request_id: request_id.to_owned(),
            session_id: session_id.to_owned(),
            run_id: run.run_id.clone(),
        });

        Ok(Some(run))
    }

    /// Drain one worker event, if any is available.
    pub async fn try_recv_agent_event(&self) -> Option<WorkerEvent> {
        let mut agent = self.ai_agent.lock().await;
        agent.as_mut()?.try_recv()
    }

    /// Long-lived task: poll the worker, publish deltas to the event bus, and
    /// finalize runs when the worker reports completion/failure.
    async fn pump_worker_events(&self) {
        let mut interval = time::interval(Duration::from_millis(EVENT_PUMP_POLL_MS));
        loop {
            interval.tick().await;

            // Drain everything queued per tick. Handling a single event per
            // tick throttled a long stream to ~40 deltas/s, which stalled the
            // worker behind its channel and made the answer stop mid-flight.
            loop {
                let Some(event) = self.try_recv_agent_event().await else {
                    break;
                };

                match &event {
                    WorkerEvent::RunCompleted { request_id, .. } => {
                        self.finish_prompt_request(request_id, "completed", None)
                            .await;
                    }
                    WorkerEvent::RunFailed {
                        request_id, error, ..
                    } => {
                        self.finish_prompt_request(request_id, "failed", Some(error))
                            .await;
                    }

                    WorkerEvent::Ready => {
                        eprintln!("Worktable: AI worker is ready");
                    }
                    WorkerEvent::WorkerError { error } => {
                        eprintln!("Worktable: AI worker error: {error}");
                        publish_worker_event(&self.events, &event);
                    }
                    _ => {
                        publish_worker_event(&self.events, &event);
                    }
                }
            }
        }
    }

    async fn finish_prompt_request(&self, request_id: &str, state: &str, error: Option<&str>) {
        let run = self.active_runs.lock().await.remove(request_id);
        let Some(run) = run else {
            // Unknown request: nothing to finalize. This is a benign race —
            // e.g. a cancelled run emits a second failure after the cancel
            // fast-path already finalized it — so log instead of surfacing a
            // spurious user-facing error.
            eprintln!("Worktable: worker finished unknown request {request_id} (state {state})");
            return;
        };
        if let Err(finish_error) = self.finish_prompt(&run, state, error).await {
            self.events.publish(WorktableEvent::AiWorkerError {
                error: format!("failed to finalize AI run: {finish_error}"),
            });
        }
    }

    pub async fn finish_prompt(
        &self,
        run: &AiRun,
        state: &str,
        error: Option<&str>,
    ) -> anyhow::Result<()> {
        if let Some(task) = self.lease_tasks.lock().await.remove(&run.run_id) {
            task.abort();
        }

        let now_ms = unix_time_ms()?;
        self.store
            .finish_run(&run.run_id, &run.session_id, state, error, now_ms)?;
        self.store
            .release_session_lease(&run.session_id, &self.owner_id, &run.run_id)?;
        if state == "completed" {
            self.events.publish(WorktableEvent::AiRunFinished {
                request_id: run.request_id.clone(),
                session_id: run.session_id.clone(),
                run_id: run.run_id.clone(),
                state: state.to_owned(),
            });
        } else {
            self.events.publish(WorktableEvent::AiRunFailed {
                request_id: run.request_id.clone(),
                session_id: run.session_id.clone(),
                run_id: run.run_id.clone(),
                error: error.unwrap_or("AI run failed").to_owned(),
            });
        }
        Ok(())
    }

    // ---- Provider / auth configuration (relayed to the worker) -------------

    pub async fn list_providers(&self) -> anyhow::Result<()> {
        self.send_to_worker(WorkerRequest::ListProviders).await
    }

    pub async fn set_api_key(&self, provider_id: &str, api_key: &str) -> anyhow::Result<()> {
        self.send_to_worker(WorkerRequest::SetApiKey {
            provider_id: provider_id.to_owned(),
            api_key: api_key.to_owned(),
        })
        .await
    }

    pub async fn set_model(&self, provider_id: &str, model_id: &str) -> anyhow::Result<()> {
        self.send_to_worker(WorkerRequest::SetModel {
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
        })
        .await
    }

    pub async fn logout_provider(&self, provider_id: &str) -> anyhow::Result<()> {
        self.send_to_worker(WorkerRequest::Logout {
            provider_id: provider_id.to_owned(),
        })
        .await
    }

    pub async fn login_oauth(&self, provider_id: &str) -> anyhow::Result<()> {
        self.send_to_worker(WorkerRequest::LoginOAuth {
            provider_id: provider_id.to_owned(),
        })
        .await
    }

    /// Abort the in-flight prompt for `session_id` (empty ids fall back to
    /// the active run). The worker handles cancellation synchronously and
    /// emits the failure event the UI already understands.
    pub async fn cancel_prompt(&self, request_id: &str, session_id: &str) -> anyhow::Result<()> {
        self.send_to_worker(WorkerRequest::Cancel {
            request_id: request_id.to_owned(),
            session_id: session_id.to_owned(),
        })
        .await
    }

    pub async fn cancel_login(&self, provider_id: &str) -> anyhow::Result<()> {
        self.send_to_worker(WorkerRequest::CancelLogin {
            provider_id: provider_id.to_owned(),
        })
        .await
    }

    pub async fn answer_auth_prompt(&self, prompt_id: &str, answer: &str) -> anyhow::Result<()> {
        self.send_to_worker(WorkerRequest::AnswerAuthPrompt {
            prompt_id: prompt_id.to_owned(),
            answer: answer.to_owned(),
        })
        .await
    }

    /// Generic config access via `wt_ai_config` table (e.g. `github_username`).
    pub fn get_config(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.store.get_config(key)
    }

    pub fn set_config(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.store.set_config(key, value)
    }

    pub fn delete_config(&self, key: &str) -> anyhow::Result<()> {
        self.store.delete_config(key)
    }

    pub fn read_provider_credential(
        &self,
        provider_id: &str,
    ) -> anyhow::Result<Option<worktable_db::ProviderCredential>> {
        self.store.read_provider_credential(provider_id)
    }

    async fn send_to_worker(&self, request: WorkerRequest) -> anyhow::Result<()> {
        let agent = self.ai_agent.lock().await;
        let agent = agent
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("AI worker has not been started"))?;
        agent.send(request)
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        if let Some(task) = self.pump_task.lock().await.take() {
            task.abort();
        }

        for task in self.lease_tasks.lock().await.values() {
            task.abort();
        }

        let agent = self.ai_agent.lock().await.take();
        if let Some(agent) = agent {
            agent.shutdown();
        }
        Ok(())
    }

    async fn start_lease_renewal(&self, run: &AiRun) {
        let store = self.store.clone();
        let owner_id = self.owner_id.clone();
        let lease_run = run.clone();
        let task = tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis((SESSION_LEASE_MS / 3) as u64));
            loop {
                interval.tick().await;

                let Ok(now_ms) = unix_time_ms() else {
                    break;
                };
                let Ok(renewed) = store.renew_session_lease(
                    &lease_run.session_id,
                    &owner_id,
                    &lease_run.run_id,
                    now_ms,
                    now_ms + SESSION_LEASE_MS,
                ) else {
                    break;
                };

                if !renewed {
                    break;
                }
            }
        });

        self.lease_tasks
            .lock()
            .await
            .insert(run.run_id.clone(), task);
    }
}

/// Run a best-effort Helix operation on a dedicated thread. Plain threads are
/// used (not `tokio::spawn`) because entry mutations can be called from GPUI's
/// scheduler, which has no Tokio reactor.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_helix_mirror(
    database_path: &str,
    task: impl FnOnce(&worktable_helix::HelixClient) + Send + 'static,
) {
    // Each mirror run is a read-modify-write cycle on one JSON graph file.
    // Concurrent inserts (e.g. a bulk star import) would otherwise interleave
    // open/merge/save and silently drop entries, so mirror runs serialize.
    static MIRROR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    let helix_path = worktable_helix::helix_path_for_sqlite(database_path);
    std::thread::spawn(move || {
        let _guard = MIRROR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let helix = worktable_helix::HelixClient::open_embedded(helix_path);
        task(&helix);
    });
}

fn unix_time_ms() -> anyhow::Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis() as i64)
}

fn publish_worker_event(events: &EventBus, event: &WorkerEvent) {
    let event = match event {
        WorkerEvent::Ready => return,
        WorkerEvent::AgentMessageDelta {
            request_id,
            session_id,
            delta,
        } => WorktableEvent::AiMessageDelta {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            delta: delta.clone(),
        },
        WorkerEvent::AgentThoughtDelta {
            request_id,
            session_id,
            delta,
        } => WorktableEvent::AiThoughtDelta {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            delta: delta.clone(),
        },
        WorkerEvent::ToolStarted {
            request_id,
            session_id,
            tool_call_id,
            name,
        } => WorktableEvent::AiToolStarted {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            tool_call_id: tool_call_id.clone(),
            name: name.clone(),
        },
        WorkerEvent::ToolFinished {
            request_id,
            session_id,
            tool_call_id,
            name,
        } => WorktableEvent::AiToolFinished {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            tool_call_id: tool_call_id.clone(),
            name: name.clone(),
        },
        WorkerEvent::Citations {
            request_id,
            session_id,
            citations,
        } => WorktableEvent::AiCitations {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            citations: citations.clone(),
        },
        WorkerEvent::RunCompleted { .. } | WorkerEvent::RunFailed { .. } => return,
        WorkerEvent::WorkerError { error } => WorktableEvent::AiWorkerError {
            error: error.clone(),
        },
        WorkerEvent::ProvidersSnapshot { snapshot } => WorktableEvent::AiProvidersSnapshot {
            snapshot: snapshot.clone(),
        },
        WorkerEvent::AuthPrompt {
            prompt_id,
            provider_id,
            prompt,
        } => WorktableEvent::AiAuthPrompt {
            prompt_id: prompt_id.clone(),
            provider_id: provider_id.clone(),
            prompt: match prompt {
                AuthPromptKind::Text {
                    message,
                    placeholder,
                } => AuthPromptKind::Text {
                    message: message.clone(),
                    placeholder: placeholder.clone(),
                },
                AuthPromptKind::Secret {
                    message,
                    placeholder,
                } => AuthPromptKind::Secret {
                    message: message.clone(),
                    placeholder: placeholder.clone(),
                },
                AuthPromptKind::ManualCode {
                    message,
                    placeholder,
                } => AuthPromptKind::ManualCode {
                    message: message.clone(),
                    placeholder: placeholder.clone(),
                },
                AuthPromptKind::Select { message, options } => AuthPromptKind::Select {
                    message: message.clone(),
                    options: options.clone(),
                },
            },
        },
        WorkerEvent::AuthNotify {
            provider_id,
            notify,
        } => WorktableEvent::AiAuthNotify {
            provider_id: provider_id.clone(),
            notify: match notify {
                AuthNotifyKind::Info { message } => AuthNotifyKind::Info {
                    message: message.clone(),
                },
                AuthNotifyKind::AuthUrl { url, instructions } => AuthNotifyKind::AuthUrl {
                    url: url.clone(),
                    instructions: instructions.clone(),
                },
                AuthNotifyKind::DeviceCode {
                    user_code,
                    verification_uri,
                    expires_in_seconds,
                } => AuthNotifyKind::DeviceCode {
                    user_code: user_code.clone(),
                    verification_uri: verification_uri.clone(),
                    expires_in_seconds: *expires_in_seconds,
                },
                AuthNotifyKind::Progress { message } => AuthNotifyKind::Progress {
                    message: message.clone(),
                },
            },
        },
        WorkerEvent::LoginResult {
            provider_id,
            ok,
            error,
        } => WorktableEvent::AiLoginResult {
            provider_id: provider_id.clone(),
            ok: *ok,
            error: error.clone(),
        },
        WorkerEvent::ConfigChanged {
            active_provider,
            active_model,
        } => WorktableEvent::AiConfigChanged {
            active_provider: active_provider.clone(),
            active_model: active_model.clone(),
        },
    };

    events.publish(event);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tokio_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    async fn connect_memory() -> WorktableRuntime {
        WorktableRuntime::connect(":memory:")
            .await
            .expect("connect in-memory")
    }

    /// Receive events until `pred` matches or the timeout elapses.
    async fn recv_event_until(
        events: &mut tokio::sync::broadcast::Receiver<WorktableEvent>,
        timeout: Duration,
        pred: impl Fn(&WorktableEvent) -> bool,
    ) -> Option<WorktableEvent> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(event)) => {
                    if pred(&event) {
                        return Some(event);
                    }
                }
                Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return None,
                Err(_) => return None,
            }
        }
    }

    #[test]
    fn prompt_without_provider_fails_and_releases_the_lease() {
        tokio_runtime().block_on(async {
            let runtime = connect_memory().await;
            runtime.start_ai_worker().await.expect("worker starts");
            let mut events = runtime.events().subscribe();

            let run = runtime
                .submit_prompt("req-1", "sess-1", "hello")
                .await
                .expect("submit succeeds")
                .expect("run started");
            assert_eq!(run.request_id, "req-1");

            // AiRunStarted is published before the terminal failure.
            let started = recv_event_until(&mut events, Duration::from_secs(5), |e| {
                matches!(e, WorktableEvent::AiRunStarted { request_id, .. } if request_id == "req-1")
            })
            .await;
            assert!(started.is_some(), "AiRunStarted should be published");

            let failed = recv_event_until(&mut events, Duration::from_secs(10), |e| {
                matches!(e, WorktableEvent::AiRunFailed { request_id, .. } if request_id == "req-1")
            })
            .await;
            let Some(WorktableEvent::AiRunFailed { error, .. }) = failed else {
                panic!("expected AiRunFailed for req-1");
            };
            assert!(
                error.contains("no AI provider configured"),
                "unexpected error: {error}"
            );

            // Regression: the lease must be released after a failed run so the
            // session is not wedged — a second prompt gets a fresh run.
            let run2 = runtime
                .submit_prompt("req-2", "sess-1", "hello again")
                .await
                .expect("second submit succeeds")
                .expect("second run started — lease was released");
            assert_eq!(run2.request_id, "req-2");

            let failed2 = recv_event_until(&mut events, Duration::from_secs(10), |e| {
                matches!(e, WorktableEvent::AiRunFailed { request_id, .. } if request_id == "req-2")
            })
            .await;
            assert!(failed2.is_some(), "second run should also terminate");

            runtime.shutdown().await.expect("shutdown");
        });
    }

    #[test]
    fn submit_prompt_without_worker_is_an_error_not_a_wedge() {
        tokio_runtime().block_on(async {
            let runtime = connect_memory().await;
            // Never started the worker.
            let result = runtime.submit_prompt("req-1", "sess-1", "hello").await;
            assert!(result.is_err(), "submitting without a worker must fail");
            // The failed send must still release the lease: starting the worker
            // afterwards and submitting works.
            runtime.start_ai_worker().await.expect("worker starts");
            let run = runtime
                .submit_prompt("req-2", "sess-1", "hello")
                .await
                .expect("submit succeeds")
                .expect("run started");
            assert_eq!(run.request_id, "req-2");
            runtime.shutdown().await.expect("shutdown");
        });
    }

    #[test]
    fn shutdown_is_idempotent() {
        tokio_runtime().block_on(async {
            let runtime = connect_memory().await;
            runtime.start_ai_worker().await.expect("worker starts");
            runtime.shutdown().await.expect("first shutdown");
            runtime
                .shutdown()
                .await
                .expect("second shutdown is a no-op");
        });
    }

    /// Reproduces the user's wedged app against a copy of their real database:
    /// a stale provider id, leftover `running` rows, and an old session lease.
    /// A fresh prompt must produce a terminal event and finalize its run row —
    /// runs must never pile up as `running` forever. Skipped when the user DB
    /// is not present (CI / other machines).
    #[test]
    fn real_db_prompt_reaches_a_terminal_event_and_finalizes() {
        let source = std::path::PathBuf::from(
            std::env::var("HOME").expect("HOME").to_string() + "/.worktable/worktable.db",
        );
        if !source.exists() {
            eprintln!("skipping: {} not found", source.display());
            return;
        }
        let copy = std::env::temp_dir().join(format!("wt-real-{}.db", uuid::Uuid::new_v4()));
        std::fs::copy(&source, &copy).expect("copy user db");

        tokio_runtime().block_on(async {
            let runtime = WorktableRuntime::connect(copy.to_str().unwrap())
                .await
                .expect("connect to the copied db");
            runtime.start_ai_worker().await.expect("worker starts");
            let mut events = runtime.events().subscribe();

            let run = runtime
                .submit_prompt("req-real-1", "wt-session", "Say hi in one word")
                .await
                .expect("submit succeeds")
                .expect("run started (stale lease must not block a fresh claim)");

            let terminal = recv_event_until(&mut events, Duration::from_secs(90), |e| {
                matches!(
                    e,
                    WorktableEvent::AiRunFinished { .. } | WorktableEvent::AiRunFailed { .. }
                )
            })
            .await;
            let Some(event) = terminal else {
                panic!(
                    "no terminal event within 90s — the app wedges exactly like the user reported"
                );
            };
            if let WorktableEvent::AiRunFailed { error, .. } = &event {
                eprintln!("terminal failure (expected with the user's stale provider): {error}");
            }

            // The run row must be finalized (completed/failed), not `running`.
            tokio::time::sleep(Duration::from_millis(200)).await;
            let state = runtime
                .store
                .run_state(&run.run_id)
                .expect("query run state")
                .expect("run row exists");
            assert!(
                state != "running",
                "run must be finalized; still '{state}' — leftover-running bug"
            );
            runtime.shutdown().await.expect("shutdown");
        });
    }
}
