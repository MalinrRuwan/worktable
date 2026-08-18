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
use worktable_db::TursoStore;
use worktable_events::{EventBus, WorktableEvent};

use crate::{
    ai_agent::AiAgentRuntime,
    worker_protocol::{WorkerEvent, WorkerRequest},
};

const SESSION_LEASE_MS: i64 = 30_000;

#[derive(Debug, Clone)]
pub struct AiRun {
    pub run_id: String,
    pub request_id: String,
    pub session_id: String,
}

#[derive(Clone)]
pub struct WorktableRuntime {
    store: TursoStore,
    ai_agent: Arc<Mutex<Option<AiAgentRuntime>>>,
    owner_id: String,
    events: EventBus,
    lease_tasks: Arc<Mutex<BTreeMap<String, JoinHandle<()>>>>,
}

impl WorktableRuntime {
    pub async fn connect(database_url: &str, auth_token: &str) -> anyhow::Result<Self> {
        let store = TursoStore::connect(database_url, auth_token).await?;
        store.migrate().await?;

        Ok(Self {
            store,
            ai_agent: Arc::new(Mutex::new(None)),
            owner_id: Uuid::new_v4().to_string(),
            events: EventBus::new(256),
            lease_tasks: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn events(&self) -> EventBus {
        self.events.clone()
    }

    pub async fn list_entries(&self, limit: usize) -> anyhow::Result<Vec<worktable_db::Entry>> {
        self.store.list_entries(limit).await
    }

    pub async fn insert_entry(&self, entry: &worktable_db::Entry) -> anyhow::Result<()> {
        self.store.insert_entry(entry).await
    }

    pub async fn delete_entry(&self, id: &str) -> anyhow::Result<()> {
        self.store.delete_entry(id).await
    }

    pub async fn start_ai_worker(
        &self,
        worker_command: &str,
        worker_args: Vec<String>,
        mut environment: BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut agent = self.ai_agent.lock().await;
        if agent.is_some() {
            return Err(anyhow::anyhow!("AI worker is already running"));
        }

        environment.insert(
            "TURSO_DATABASE_URL".to_owned(),
            self.store.database_url().to_owned(),
        );
        environment.insert(
            "TURSO_AUTH_TOKEN".to_owned(),
            self.store.auth_token().to_owned(),
        );

        let worker = AiAgentRuntime::start(worker_command, worker_args, environment).await?;
        *agent = Some(worker);
        Ok(())
    }

    pub async fn submit_prompt(
        &self,
        request_id: &str,
        session_id: &str,
        content: &str,
    ) -> anyhow::Result<Option<AiRun>> {
        let now_ms = unix_time_ms()?;
        self.store.ensure_session(session_id, None, now_ms).await?;

        let run_id = Uuid::new_v4().to_string();
        let claimed = self
            .store
            .claim_session_lease(
                session_id,
                &self.owner_id,
                &run_id,
                now_ms,
                now_ms + SESSION_LEASE_MS,
            )
            .await?;
        if !claimed {
            return Ok(None);
        }

        let started = self
            .store
            .begin_run(&run_id, session_id, request_id, now_ms)
            .await?;
        if !started {
            self.store
                .release_session_lease(session_id, &self.owner_id, &run_id)
                .await?;
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
            let finish_error = self
                .store
                .finish_run(
                    &run_id,
                    session_id,
                    "failed",
                    Some(&error.to_string()),
                    unix_time_ms()?,
                )
                .await;
            self.store
                .release_session_lease(session_id, &self.owner_id, &run_id)
                .await?;
            finish_error?;
            return Err(error);
        }

        let run = AiRun {
            run_id,
            request_id: request_id.to_owned(),
            session_id: session_id.to_owned(),
        };
        self.start_lease_renewal(&run).await;

        self.events.publish(WorktableEvent::AiRunStarted {
            request_id: request_id.to_owned(),
            session_id: session_id.to_owned(),
            run_id: run.run_id.clone(),
        });

        Ok(Some(run))
    }

    pub async fn try_recv_agent_event(&self) -> Option<WorkerEvent> {
        let mut agent = self.ai_agent.lock().await;
        let event = agent.as_mut()?.try_recv();
        if let Some(event) = &event {
            publish_worker_event(&self.events, event);
        }
        event
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
            .finish_run(&run.run_id, &run.session_id, state, error, now_ms)
            .await?;
        self.store
            .release_session_lease(&run.session_id, &self.owner_id, &run.run_id)
            .await?;
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

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        for task in self.lease_tasks.lock().await.values() {
            task.abort();
        }

        let agent = self.ai_agent.lock().await.take();
        if let Some(agent) = agent {
            agent.shutdown().await?;
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
                let Ok(renewed) = store
                    .renew_session_lease(
                        &lease_run.session_id,
                        &owner_id,
                        &lease_run.run_id,
                        now_ms,
                        now_ms + SESSION_LEASE_MS,
                    )
                    .await
                else {
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
    };

    events.publish(event);
}
