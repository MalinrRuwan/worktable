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
    pi_agent::PiAgentRuntime,
    worker_protocol::{WorkerEvent, WorkerRequest},
};

const SESSION_LEASE_MS: i64 = 30_000;
const EVENT_PUMP_POLL_MS: u64 = 25;

#[derive(Debug, Clone)]
pub struct AiRun {
    pub run_id: String,
    pub request_id: String,
    pub session_id: String,
}

#[derive(Clone)]
pub struct WorktableRuntime {
    store: SqliteStore,
    ai_agent: Arc<Mutex<Option<PiAgentRuntime>>>,
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
        self.store.insert_entry(entry)
    }

    pub async fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
        self.store.delete_entry(id)
    }

    pub async fn start_ai_worker(&self) -> anyhow::Result<()> {
        let mut agent = self.ai_agent.lock().await;
        if agent.is_some() {
            return Err(anyhow::anyhow!("AI worker is already running"));
        }

        let worker = PiAgentRuntime::start(self.store.clone());
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

            let event = match self.try_recv_agent_event().await {
                Some(event) => event,
                None => continue,
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

    async fn finish_prompt_request(&self, request_id: &str, state: &str, error: Option<&str>) {
        let run = self.active_runs.lock().await.remove(request_id);
        let Some(run) = run else {
            // Unknown request: nothing to finalize, but still surface the event.
            self.events.publish(WorktableEvent::AiWorkerError {
                error: format!("worker finished unknown request {request_id}"),
            });
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
