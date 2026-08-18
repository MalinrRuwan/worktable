use serde::{Deserialize, Serialize};

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
}
