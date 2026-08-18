use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use agentos_client::{
    AgentOs, AgentOsConfig, ProcessOutput, ProcessStream, SpawnOptions, SpawnStdio, StdinInput,
    Subscription,
};
use anyhow::Context;
use tokio::sync::mpsc;

use crate::worker_protocol::{WorkerEvent, WorkerRequest, decode_event, encode_request};

pub struct AiAgentRuntime {
    agent_os: AgentOs,
    worker_pid: Option<u32>,
    output_rx: mpsc::Receiver<WorkerEvent>,
    _output_subscription: Subscription,
}

impl AiAgentRuntime {
    pub async fn start(
        worker_command: &str,
        worker_args: Vec<String>,
        environment: BTreeMap<String, String>,
    ) -> anyhow::Result<Self> {
        let agent_os = AgentOs::create(AgentOsConfig::default())
            .await
            .context("failed to start AgentOS")?;

        let process = agent_os
            .spawn_process(
                worker_command,
                worker_args,
                SpawnOptions {
                    env: environment,
                    stdio: Some(SpawnStdio::Pipe),
                    stream_stdin: Some(true),
                    ..SpawnOptions::default()
                },
            )
            .context("failed to start Worktable Pi worker")?;

        let (event_tx, event_rx) = mpsc::channel(256);
        let event_tx_for_output = event_tx.clone();
        let stdout_buffer = Arc::new(Mutex::new(Vec::new()));
        let stdout_buffer_for_output = Arc::clone(&stdout_buffer);

        let output_subscription = agent_os
            .on_process_output(process.pid, move |output| {
                forward_process_output(output, &event_tx_for_output, &stdout_buffer_for_output);
            })
            .context("failed to subscribe to Pi worker output")?;

        Ok(Self {
            agent_os,
            worker_pid: Some(process.pid),
            output_rx: event_rx,
            _output_subscription: output_subscription,
        })
    }

    pub fn try_recv(&mut self) -> Option<WorkerEvent> {
        self.output_rx.try_recv().ok()
    }

    pub fn send(&self, request: WorkerRequest) -> anyhow::Result<()> {
        let pid = self.worker_pid.context("Pi worker is not running")?;

        let mut line = encode_request(&request)?;
        line.push('\n');

        self.agent_os
            .write_process_stdin(pid, StdinInput::Text(line))
            .context("failed to send request to Pi worker")?;

        Ok(())
    }

    pub fn stop(&self) -> anyhow::Result<()> {
        if let Some(pid) = self.worker_pid {
            self.agent_os
                .stop_process(pid)
                .context("failed to stop Pi worker")?;
        }

        Ok(())
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.stop()?;
        self.worker_pid = None;
        self.agent_os
            .shutdown()
            .await
            .context("failed to shut down AgentOS")?;
        Ok(())
    }
}

fn forward_process_output(
    output: ProcessOutput,
    event_tx: &mpsc::Sender<WorkerEvent>,
    stdout_buffer: &Arc<Mutex<Vec<u8>>>,
) {
    if !matches!(output.stream, ProcessStream::Stdout) {
        let error = String::from_utf8_lossy(&output.data).trim().to_owned();
        if !error.is_empty() {
            let _ = event_tx.try_send(WorkerEvent::WorkerError { error });
        }
        return;
    }

    let Ok(mut buffer) = stdout_buffer.lock() else {
        let _ = event_tx.try_send(WorkerEvent::WorkerError {
            error: "failed to lock worker stdout buffer".to_owned(),
        });
        return;
    };

    buffer.extend_from_slice(&output.data);

    while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
        let line: Vec<u8> = buffer.drain(..=newline).collect();
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);

        if line.is_empty() {
            continue;
        }

        match decode_event(line) {
            Ok(event) => {
                let _ = event_tx.try_send(event);
            }
            Err(error) => {
                let _ = event_tx.try_send(WorkerEvent::WorkerError {
                    error: format!("invalid worker event: {error}"),
                });
            }
        }
    }
}
