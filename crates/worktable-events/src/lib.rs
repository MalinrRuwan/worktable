use serde::{Deserialize, Serialize};

/// One `search_knowledge` hit the model may cite as `[n]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeCitation {
    /// Marker number used in the model's answer.
    pub n: u32,
    /// Entry id in the store, so the app can reveal the source in-app.
    pub entry_id: String,
    /// Human-readable source label (title, or a content excerpt).
    pub label: String,
    /// Short content excerpt shown in the citation preview on hover.
    pub snippet: String,
    /// Short display host: URL host for links, otherwise the entry source.
    pub host: String,
    /// External URL when the entry points at one; empty for plain notes.
    pub url: String,
}
use tokio::sync::broadcast;

/// A single model advertised by an AI provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    /// Optional catalog-owned service heading, such as OpenCode Go or Zen.
    /// Presentation metadata only: `id` remains the provider's dispatch id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// API-advertised protocol metadata; never inferred from a model's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// A selectable subset of a provider's models. OpenCode, for example, offers
/// its Go subscription catalog and its Zen pay-as-you-go catalog from one
/// key; providers without model groups send an empty list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderGroup {
    pub id: String,
    pub name: String,
    pub enabled: bool,
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
    /// Model groups the user can enable or disable (empty for most providers).
    #[serde(default)]
    pub groups: Vec<ProviderGroup>,
    /// The models this provider currently offers (already filtered by the
    /// enabled groups).
    pub models: Vec<ModelInfo>,
    #[serde(default)]
    pub models_loading: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_error: Option<String>,
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
    /// A tool call finished executing.
    AiToolFinished {
        request_id: String,
        session_id: String,
        tool_call_id: String,
        name: String,
    },
    /// Numbered knowledge citations for the answer being produced.
    AiCitations {
        request_id: String,
        session_id: String,
        citations: Vec<KnowledgeCitation>,
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{EventBus, ModelInfo, WorktableEvent};

    #[test]
    fn model_groups_are_optional_and_keep_dispatch_ids_intact() {
        let old: ModelInfo =
            serde_json::from_str(r#"{"id":"gpt-5-mini","name":"GPT-5 mini"}"#).unwrap();
        assert!(old.group.is_none());
        assert!(
            !serde_json::to_value(&old)
                .unwrap()
                .as_object()
                .unwrap()
                .contains_key("group")
        );

        let grouped: ModelInfo = serde_json::from_str(
            r#"{"id":"gpt-5-mini","name":"GPT-5 mini","group":"OpenCode Zen"}"#,
        )
        .unwrap();
        assert_eq!(grouped.id, old.id);
        let restored: ModelInfo =
            serde_json::from_str(&serde_json::to_string(&grouped).unwrap()).unwrap();
        assert_eq!(restored.group.as_deref(), Some("OpenCode Zen"));
    }

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

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_tests {
    use super::{EventBus, WorktableEvent};
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn extensions_can_receive_typed_events_sync() {
        let bus = EventBus::new(4);
        let _events = bus.subscribe();
        let count = bus.publish(WorktableEvent::TextAdded {
            item_id: "item-1".to_owned(),
            content: "hello".to_owned(),
            source: "test".to_owned(),
        });
        assert!(count <= 1);
    }
}
