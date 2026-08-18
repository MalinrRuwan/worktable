import { createInterface } from "node:readline";

import {
  Agent,
  JsonlSessionRepo,
  type AgentMessage,
  type Entry,
} from "@earendil-works/pi-agent-core";
import { builtinModels } from "@earendil-works/pi-ai/providers/all";

import {
  decodeRequest,
  writeEvent,
  type WorkerRequest,
} from "./protocol.js";
import { migrateTursoPiFiles } from "./turso-jsonl.js";

const WORKTREE_CWD = "/worktable";
const SESSIONS_ROOT = "/worktable/sessions";
const databaseUrl = requiredEnvironment("TURSO_DATABASE_URL");
const databaseToken = requiredEnvironment("TURSO_AUTH_TOKEN");

const filesystem = await migrateTursoPiFiles(databaseUrl, databaseToken);
const repository = new JsonlSessionRepo({
  fs: filesystem.asPiFileSystem(),
  sessionsRoot: SESSIONS_ROOT,
});
const models = builtinModels();
const activeAgents = new Map<string, Agent>();
const cancelledRequests = new Set<string>();
const sessionQueues = new Map<string, Promise<void>>();

writeEvent({ type: "ready" });

const input = createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
});

const pending = new Set<Promise<void>>();
let shuttingDown = false;

for await (const line of input) {
  if (!line.trim()) {
    continue;
  }

  let request: WorkerRequest;
  try {
    request = decodeRequest(line);
  } catch (error) {
    writeEvent({
      type: "worker_error",
      error: `invalid worker request: ${errorMessage(error)}`,
    });
    continue;
  }

  if (request.type === "shutdown") {
    shuttingDown = true;
    break;
  }

  if (request.type === "cancel") {
    const agent = activeAgents.get(request.request_id);
    if (agent) {
      agent.abort();
    } else {
      cancelledRequests.add(request.request_id);
    }
    continue;
  }

  const previous = sessionQueues.get(request.session_id) ?? Promise.resolve();
  const run = previous
    .catch(() => undefined)
    .then(() => runPrompt(request));
  sessionQueues.set(request.session_id, run);
  pending.add(run);
  void run.finally(() => {
    pending.delete(run);
    if (sessionQueues.get(request.session_id) === run) {
      sessionQueues.delete(request.session_id);
    }
  });
}

if (shuttingDown) {
  await Promise.allSettled(pending);
}

async function runPrompt(request: Extract<WorkerRequest, { type: "prompt" }>) {
  if (cancelledRequests.delete(request.request_id)) {
    writeEvent({
      type: "run_failed",
      request_id: request.request_id,
      session_id: request.session_id,
      error: "aborted before execution",
    });
    return;
  }

  try {
    const session = await getOrCreateSession(request.session_id);
    const messages = await loadMessages(session);
    const model = resolveModel();

    const agent = new Agent({
      initialState: {
        model,
        messages,
      },
      sessionId: request.session_id,
      streamFn: models.streamSimple.bind(models),
    });
    activeAgents.set(request.request_id, agent);

    agent.subscribe(async (event) => {
      if (event.type === "message_update") {
        if (event.assistantMessageEvent.type === "text_delta") {
          writeEvent({
            type: "agent_message_delta",
            request_id: request.request_id,
            session_id: request.session_id,
            delta: event.assistantMessageEvent.delta,
          });
        } else if (event.assistantMessageEvent.type === "thinking_delta") {
          writeEvent({
            type: "agent_thought_delta",
            request_id: request.request_id,
            session_id: request.session_id,
            delta: event.assistantMessageEvent.delta,
          });
        }
      } else if (event.type === "tool_execution_start") {
        writeEvent({
          type: "tool_started",
          request_id: request.request_id,
          session_id: request.session_id,
          tool_call_id: event.toolCallId,
          name: event.toolName,
        });
      } else if (event.type === "message_end") {
        await session.appendMessage(event.message);
      }
    });

    await agent.prompt(request.content);
    writeEvent({
      type: "run_completed",
      request_id: request.request_id,
      session_id: request.session_id,
    });
  } catch (error) {
    writeEvent({
      type: "run_failed",
      request_id: request.request_id,
      session_id: request.session_id,
      error: errorMessage(error),
    });
  } finally {
    activeAgents.delete(request.request_id);
  }
}

async function getOrCreateSession(sessionId: string) {
  const metadata = (await repository.list({ cwd: WORKTREE_CWD })).find(
    (candidate) => candidate.id === sessionId,
  );

  if (metadata) {
    return repository.open(metadata);
  }

  return repository.create({
    id: sessionId,
    cwd: WORKTREE_CWD,
  });
}

async function loadMessages(session: Awaited<ReturnType<typeof getOrCreateSession>>) {
  const entries = await session.findEntries({
    order: "oldestFirst",
  });

  return entries.flatMap((entry: Entry): AgentMessage[] =>
    entry.type === "message" ? [entry.message] : [],
  );
}

function resolveModel() {
  const provider = requiredEnvironment("PI_PROVIDER");
  const modelId = requiredEnvironment("PI_MODEL");
  const model = models.getModel(provider, modelId);

  if (!model) {
    throw new Error(`Pi model is not available: ${provider}/${modelId}`);
  }

  return model;
}

function requiredEnvironment(name: string): string {
  const value = process.env[name];
  if (!value) {
    throw new Error(`${name} must be set for the Worktable Pi worker`);
  }

  return value;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
