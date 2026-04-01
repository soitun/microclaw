# Concurrency and Responsiveness

This note answers a recurring architecture question: does MicroClaw still behave like a single blocking agent loop, or can it already move toward a more Spacebot-like non-blocking design?

Short answer:

- one individual run is still sequential
- the runtime as a whole is already multi-lane
- the main existing non-blocking primitive is session-native subagents plus streamed web runs

## What is concurrent today

MicroClaw is not one global blocking session.

- Channel runtimes are started independently from `src/runtime.rs`, so Telegram, Discord, Slack, Web, and other adapters do not all wait on one shared blocking loop.
- The web streaming path creates background run tasks and emits replayable SSE events from `src/web/stream.rs`.
- Scheduler jobs and reflector passes are spawned independently from `src/scheduler.rs`.
- Session-native subagents are accepted immediately, then continue in the background via the runtime in `src/tools/subagents.rs`.

In practice, this means one long-running web request, scheduled task, or subagent run does not freeze the whole process.

## What is still sequential

Inside one single run, the core loop in `src/agent_engine.rs` still executes in order:

1. load session and memory
2. call the model
3. execute tool calls
4. feed results back
5. compact or persist state

So MicroClaw is not yet a fully parallel actor mesh where one conversation can freely keep talking while its main run is still mid-turn.

## Existing non-blocking building blocks

### Web streaming

The web surface is the clearest example of current responsiveness:

- `POST /api/send_stream` returns a `run_id` immediately
- `/api/stream` replays `status`, `tool_start`, `tool_result`, `delta`, and terminal events
- clients can reconnect and resume from the last seen event id

This already gives MicroClaw a non-blocking UX on the web surface.

### Background scheduler and reflector

Two expensive classes of work are already split out of the interactive turn path:

- scheduled tasks
- structured-memory reflection

That avoids forcing every chat turn to pay the latency of those jobs inline.

### Session-native subagents

The current answer to "can it behave more like Spacebot" is mostly "yes, through subagents, but with scoped limits".

- `sessions_spawn` returns immediately with `status=accepted`
- the child run progresses through `accepted -> queued -> running -> completed|failed|timed_out|cancelled`
- completion can be announced back to the parent chat
- operators can inspect or control the run with `subagents_list`, `subagents_info`, `subagents_log`, `subagents_focus`, `subagents_send`, and `subagents_kill`

This is not the same as fully decomposing the whole product into many independent worker processes, but it already establishes a durable background execution lane.

## Limits that still matter

- Parallel tool execution inside one turn is still not the default model.
- Most chat adapters still handle one inbound message as one primary run, even if they keep typing indicators or progress pings alive.
- Streaming visibility is strongest on the web surface. Chat channels are more limited by transport capabilities.
- Subagent concurrency is intentionally bounded by `subagents.max_concurrent`, `subagents.max_active_per_chat`, timeout settings, and spawn-depth limits.

## About binary size

The 30 MB binary size is mostly orthogonal to this question.

- a single binary can still be highly concurrent
- a small binary can still serialize all useful work through one blocking lane

For MicroClaw, the real architecture question is whether expensive work is isolated into independent async lanes or background runs. That part is already improving even without splitting the product into many deployables.

## Practical guidance

If you want the most non-blocking behavior today:

- prefer the Web UI or Web operator API for streaming status
- offload longer work with `sessions_spawn`
- keep subagent completion announces enabled
- tune `subagents.max_concurrent`, `subagents.max_active_per_chat`, and `subagents.run_timeout_secs`

## Near-term direction

The current roadmap still points toward:

- safer parallelism within a single turn where possible
- stronger parent/child contracts for orchestration-heavy flows
- richer observability for multi-run execution
- deeper thread-bound routing and fan-out/fan-in patterns for subagents

So the right characterization is not "MicroClaw is still a single blocking loop". It is "the main agent turn is sequential, but the runtime already has multiple non-blocking lanes, and subagents are the bridge toward a more concurrent architecture."
