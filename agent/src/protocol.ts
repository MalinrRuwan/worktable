export type WorkerRequest =
  | {
      type: "prompt";
      request_id: string;
      session_id: string;
      content: string;
    }
  | {
      type: "cancel";
      request_id: string;
      session_id: string;
    }
  | {
      type: "shutdown";
    };

export type WorkerEvent =
  | {
      type: "ready";
    }
  | {
      type: "agent_message_delta";
      request_id: string;
      session_id: string;
      delta: string;
    }
  | {
      type: "agent_thought_delta";
      request_id: string;
      session_id: string;
      delta: string;
    }
  | {
      type: "tool_started";
      request_id: string;
      session_id: string;
      tool_call_id: string;
      name: string;
    }
  | {
      type: "run_completed";
      request_id: string;
      session_id: string;
    }
  | {
      type: "run_failed";
      request_id: string;
      session_id: string;
      error: string;
    }
  | {
      type: "worker_error";
      error: string;
    };

export function decodeRequest(line: string): WorkerRequest {
  const request = JSON.parse(line) as WorkerRequest;
  if (!request || typeof request !== "object" || typeof request.type !== "string") {
    throw new Error("worker request must be a tagged JSON object");
  }

  return request;
}

export function writeEvent(event: WorkerEvent): void {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}
