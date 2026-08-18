use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

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
