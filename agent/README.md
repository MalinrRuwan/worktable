# Worktable Pi worker

This package is the process that runs inside the AgentOS boundary. It uses the
original `@earendil-works/pi-agent-core` and `@earendil-works/pi-ai` packages;
it does not use the AgentOS Pi package.

The worker's session repository is the Pi JSONL repository backed by a virtual
filesystem whose rows live in the same remote Turso database as Worktable.
AgentOS contributes process isolation only. It is not given a database
descriptor, and it does not own session state.

Required environment:

- `TURSO_DATABASE_URL`
- `TURSO_AUTH_TOKEN`
- `PI_PROVIDER`
- `PI_MODEL`

Build this package and pass the resulting Node worker command to the Rust
`AiAgentRuntime`. The Rust side sends and receives newline-delimited JSON over
the AgentOS-managed process pipes.
