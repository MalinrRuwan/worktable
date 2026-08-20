use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// A single model advertised by an AI provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
}

/// A provider known to the Pi worker, with its auth capabilities and models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub supports_api_key: bool,
    pub supports_oauth: bool,
    pub api_key_set: bool,
    pub oauth_set: bool,
    pub models: Vec<ModelInfo>,
}

/// A snapshot of every provider (sent in response to `ListProviders`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvidersSnapshot {
    pub providers: Vec<ProviderInfo>,
    pub active_provider: Option<String>,
    pub active_model: Option<String>,
}

/// Interactive prompt sent from the worker to the app during login.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthPromptKind {
    Text {
        message: String,
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        placeholder: Option<String>,
    },
    ManualCode {
        message: String,
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<SelectOption>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// Non-interactive notices from the worker during login (URLs, device codes…).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthNotifyKind {
    Info {
        message: String,
    },
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
        expires_in_seconds: Option<i64>,
    },
    Progress {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorktableEvent {
    TextAdded {
        item_id: String,
        content: String,
        source: String,
    },
    LinkAdded {
        item_id: String,
        url: String,
        source: String,
    },
    ImageAdded {
        item_id: String,
        mime_type: String,
        blob_ref: String,
        source: String,
    },
    AiRunStarted {
        request_id: String,
        session_id: String,
        run_id: String,
    },
    AiMessageDelta {
        request_id: String,
        session_id: String,
        delta: String,
    },
    AiThoughtDelta {
        request_id: String,
        session_id: String,
        delta: String,
    },
    AiToolStarted {
        request_id: String,
        session_id: String,
        tool_call_id: String,
        name: String,
    },
    AiRunFinished {
        request_id: String,
        session_id: String,
        run_id: String,
        state: String,
    },
    AiRunFailed {
        request_id: String,
        session_id: String,
        run_id: String,
        error: String,
    },
    AiWorkerError {
        error: String,
    },
    /// Full provider catalog + current selection, after any config change.
    AiProvidersSnapshot {
        snapshot: ProvidersSnapshot,
    },
    /// Interactive prompt the user must answer during login.
    AiAuthPrompt {
        prompt_id: String,
        provider_id: String,
        prompt: AuthPromptKind,
    },
    /// Non-interactive login notice (auth URL, device code, progress…).
    AiAuthNotify {
        provider_id: String,
        notify: AuthNotifyKind,
    },
    /// Outcome of an OAuth login attempt.
    AiLoginResult {
        provider_id: String,
        ok: bool,
        error: Option<String>,
    },
    /// The active provider/model changed.
    AiConfigChanged {
        active_provider: Option<String>,
        active_model: Option<String>,
    },
}

#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<WorktableEvent>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<WorktableEvent> {
        self.sender.subscribe()
    }

    pub fn publish(&self, event: WorktableEvent) -> usize {
        self.sender.send(event).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{EventBus, WorktableEvent};

    #[tokio::test]
    async fn extensions_can_receive_typed_events() {
        let bus = EventBus::new(4);
        let mut events = bus.subscribe();

        bus.publish(WorktableEvent::TextAdded {
            item_id: "item-1".to_owned(),
            content: "hello".to_owned(),
            source: "test".to_owned(),
        });

        let event = events.recv().await.expect("event should be delivered");
        assert!(matches!(event, WorktableEvent::TextAdded { .. }));
    }
}
