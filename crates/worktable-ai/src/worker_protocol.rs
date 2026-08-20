use serde::{Deserialize, Serialize};

use worktable_events::{AuthNotifyKind, AuthPromptKind, ProvidersSnapshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerRequest {
    Prompt {
        request_id: String,
        session_id: String,
        content: String,
    },
    Cancel {
        request_id: String,
        session_id: String,
    },
    Shutdown,
    /// Ask the worker for a full provider snapshot (catalog + auth status).
    ListProviders,
    /// Store an API key for a provider and make it the active provider.
    SetApiKey {
        provider_id: String,
        api_key: String,
    },
    /// Select the active provider + model.
    SetModel {
        provider_id: String,
        model_id: String,
    },
    /// Remove the stored credential for a provider.
    Logout {
        provider_id: String,
    },
    /// Start an OAuth login flow for a provider. The worker streams
    /// `AuthPrompt` / `AuthNotify` events until it replies with `LoginResult`.
    #[serde(rename = "login_oauth")]
    LoginOAuth {
        provider_id: String,
    },
    /// Abort an in-progress `LoginOAuth` for a provider.
    CancelLogin {
        provider_id: String,
    },
    /// Answer an outstanding `AuthPrompt`.
    AnswerAuthPrompt {
        prompt_id: String,
        answer: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    Ready,
    AgentMessageDelta {
        request_id: String,
        session_id: String,
        delta: String,
    },
    AgentThoughtDelta {
        request_id: String,
        session_id: String,
        delta: String,
    },
    ToolStarted {
        request_id: String,
        session_id: String,
        tool_call_id: String,
        name: String,
    },
    RunCompleted {
        request_id: String,
        session_id: String,
    },
    RunFailed {
        request_id: String,
        session_id: String,
        error: String,
    },
    WorkerError {
        error: String,
    },
    ProvidersSnapshot {
        snapshot: ProvidersSnapshot,
    },
    AuthPrompt {
        prompt_id: String,
        provider_id: String,
        prompt: AuthPromptKind,
    },
    AuthNotify {
        provider_id: String,
        notify: AuthNotifyKind,
    },
    LoginResult {
        provider_id: String,
        ok: bool,
        error: Option<String>,
    },
    ConfigChanged {
        active_provider: Option<String>,
        active_model: Option<String>,
    },
}

pub fn encode_request(request: &WorkerRequest) -> serde_json::Result<String> {
    serde_json::to_string(request)
}

pub fn decode_event(line: &[u8]) -> serde_json::Result<WorkerEvent> {
    serde_json::from_slice(line)
}

#[cfg(test)]
mod tests {
    use super::{WorkerEvent, WorkerRequest, decode_event, encode_request};
    use worktable_events::{ModelInfo, ProviderInfo, ProvidersSnapshot};

    #[test]
    fn worker_request_round_trips_as_json() {
        let request = WorkerRequest::Prompt {
            request_id: "run-1".to_owned(),
            session_id: "session-1".to_owned(),
            content: "hello".to_owned(),
        };

        let encoded = encode_request(&request).expect("request should encode");
        let decoded: WorkerRequest = serde_json::from_str(&encoded).expect("request should decode");

        assert!(matches!(decoded, WorkerRequest::Prompt { .. }));
    }

    #[test]
    fn worker_event_decodes_from_json() {
        let event = decode_event(br#"{"type":"ready"}"#).expect("event should decode");

        assert!(matches!(event, WorkerEvent::Ready));
    }

    #[test]
    fn provider_snapshot_round_trips() {
        let snapshot = ProvidersSnapshot {
            providers: vec![ProviderInfo {
                id: "anthropic".to_owned(),
                name: "Anthropic".to_owned(),
                supports_api_key: true,
                supports_oauth: true,
                api_key_set: true,
                oauth_set: false,
                models: vec![ModelInfo {
                    id: "claude-sonnet-4-5".to_owned(),
                    name: "Claude Sonnet 4.5".to_owned(),
                }],
            }],
            active_provider: Some("anthropic".to_owned()),
            active_model: Some("claude-sonnet-4-5".to_owned()),
        };

        let event = WorkerEvent::ProvidersSnapshot {
            snapshot: snapshot.clone(),
        };
        let encoded = serde_json::to_string(&event).expect("event should encode");
        let decoded: WorkerEvent = serde_json::from_str(&encoded).expect("event should decode");

        match decoded {
            WorkerEvent::ProvidersSnapshot { snapshot: decoded } => {
                assert_eq!(decoded.providers[0].id, "anthropic");
                assert_eq!(decoded.providers[0].models[0].id, "claude-sonnet-4-5");
                assert!(decoded.providers[0].api_key_set);
            }
            _ => panic!("expected providers snapshot"),
        }
    }
}
