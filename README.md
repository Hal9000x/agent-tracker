# Agent Tracker

Agent Tracker records every step an AI agent takes after you send it a message, then lets you inspect the run like a browser's Network panel or like an n8n/ComfyUI node graph.

- **Network view**: a historic log of spans with Name, Status, Type, Initiator, Size, Time and Waterfall columns. It has an overview timeline you can drag to clip a time range, type chips and a `key:value` filter, and a details pane with the Headers, Payload, Preview, Response, Initiator and Timing tabs.
- **Graph view**: the run drawn as a flow. A single agent reads as a straight chain. A swarm shows splits, forks, joins, parallel branches and messages between agents. Each agent's steps sit inside a group box. Click any node to light up everything upstream and downstream of it. Switch to **Agents** to fold steps into one node per agent and see how the agents interacted.

Data arrives live over Server-Sent Events, so runs appear while they are still going.

## Quick start

Requires Rust 1.85+ and Node 22.18+ (Node 24 recommended).

```bash
cd web && npm install && npm run build && cd ..
```

```bash
cargo run --release --manifest-path daemon/Cargo.toml
```

Open http://127.0.0.1:4318, then send it something:

```bash
node clients/typescript/examples/demo.ts
```

The demo plays three runs in real time: a single agent with a tool call, a research swarm (split, three parallel researchers, join, a fork into reviewers with a message between them), and a support handoff with a failing tool. `DEMO_SPEED=4` speeds it up.

The release binary embeds the UI, so `daemon/target/release/agent-tracker` is all you need to ship. Flags: `--host` (default `127.0.0.1`), `--port` (default `4318`), `--data` (default `./data`).

## Sending data

There are four ways in. They all produce the same spans.

### Claude Code

Merge [`integrations/claude-code/settings.json`](integrations/claude-code/settings.json) into a Claude Code settings file (`~/.claude/settings.json` for every project, or `.claude/settings.local.json` for one). It adds `http` hooks that post each event to the tracker.

Each prompt you submit becomes one trace: the prompt, the main agent, every tool call, and each subagent nested under the `Agent` tool call that started it. Parallel subagents show up as a fork. If the tracker is not running, Claude Code prints a non-blocking "hook failed" notice and carries on.

### OpenTelemetry

Point any OTLP/HTTP exporter at the tracker. Protobuf and JSON both work.

```bash
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces
```

Spans are classified with the [GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/) (`gen_ai.operation.name`: `invoke_agent`, `chat`, `execute_tool`, …), falling back to OpenInference's `openinference.span.kind`. Model, token usage, input/output messages, errors and span links are picked up. Compressed payloads are rejected; set `OTEL_EXPORTER_OTLP_COMPRESSION=none`.

### TypeScript

[`clients/typescript/tracker.ts`](clients/typescript/tracker.ts) is one dependency-free file. Spans nest by async context, so `Promise.all` inside a `fork` or `split` gives parallel branches.

```ts
import { Tracker } from "./tracker.ts";

const t = new Tracker(); // AGENT_TRACKER_URL overrides the default http://127.0.0.1:4318
await t.trace("Plan a trip", prompt, () =>
  t.agent("planner", { input: prompt }, async () => {
    const plan = await t.llm("plan", { model: "claude-sonnet-5-5" }, async (s) => {
      const r = await callModel(prompt);
      s.tokens(r.usage.input_tokens, r.usage.output_tokens);
      return r.content;
    });
    return t.split("research", { input: plan.topics }, () =>
      Promise.all(plan.topics.map((topic) => t.agent(`researcher:${topic}`, { input: topic }, () => research(topic)))),
    );
  }),
);
await t.flush();
```

A body's return value becomes the span's output, and a thrown error marks the span failed. Pass `links: [span]` to record a message from one agent's span to another.

### Python

[`clients/python/agent_tracker.py`](clients/python/agent_tracker.py) is one standard-library-only file. It uses context managers and `contextvars`, so `asyncio.gather` inside a `split` or `fork` gives parallel branches. A background thread sends updates, so tracking never blocks or raises into your agent.

```python
from agent_tracker import Tracker

t = Tracker()
with t.trace("Plan a trip", input=prompt):
    with t.agent("planner", input=prompt):
        with t.llm("plan", model="claude-sonnet-5-5") as s:
            s.output(answer).tokens(412, 58)
```

See [`clients/python/examples/demo.py`](clients/python/examples/demo.py) for a split over parallel planners.

### Raw HTTP

`POST /api/spans` takes one span update or an array of them. Updates with the same `trace_id` + `span_id` merge, so you can report a span when it starts and again when it ends, in either order.

| Field | Type | Notes |
|---|---|---|
| `trace_id`, `span_id` | string | required |
| `parent_id` | string | the span that started this one |
| `kind` | `message` `agent` `llm` `tool` `split` `fork` `join` `step` | children of `split`/`fork` are drawn as parallel branches; unknown kinds become `step` |
| `name`, `agent` | string | `agent` is inherited by descendants for display |
| `start`, `end` | number | epoch milliseconds; a span without `end` is running |
| `status` | `running` `ok` `error` | defaults from `end`; a late `running` never reopens an ended span |
| `error` | string | |
| `input`, `output` | any JSON | |
| `model`, `tokens_in`, `tokens_out` | | |
| `attrs` | object | merged key by key |
| `links` | string[] | span ids this span received output from; drawn as dashed message edges |

Other endpoints: `GET /api/traces`, `GET /api/traces/{id}`, `GET /api/stream` (SSE), `POST /v1/traces` (OTLP), `POST /api/hooks/claude-code`.

## Using the views

**Filter** (Network view, and Highlight in the Graph view): plain words match names, agents and payloads. `agent:`, `kind:`, `status:`, `model:` and `name:` match one field, and `-` excludes. For example, `agent:researcher -status:ok`. Ctrl-click type chips to combine them.

**Clip**: drag across the overview timeline. Drag the edges to resize the clip, drag inside it to move it, and double-click to clear it. The table rescales to the clip, and the Graph view dims spans outside it. **Export** downloads the visible spans as JSON.

**How the graph is drawn**: a span leads into its children. Children that overlap in time run in parallel, and children that follow one another chain in sequence. Children of a `split` or `fork` are always parallel. The last steps of one stage feed the next. In the Agents view, an edge back to an agent that contains the source is drawn as an arc labelled *returns*, and it is left out of the click-to-trace highlight.

## Layout

```
daemon/        Rust server (axum): ingest, store, SSE, OTLP and Claude Code adapters, embedded UI
web/           React + TypeScript UI (Vite, React Flow, dagre)
clients/       TypeScript and Python clients, with demos
integrations/  Claude Code hook settings
```

Spans live in memory and are appended to `data/spans.jsonl`. The log is replayed on startup.

## Development

```bash
cargo test --manifest-path daemon/Cargo.toml
```

```bash
cd web && npm test
```

For UI work, run the daemon, then `npm run dev` in `web/`. Vite proxies `/api` and `/v1` to the daemon on port 4318 (override with `AGENT_TRACKER_URL`).

## Limits

- No authentication. The daemon binds to `127.0.0.1` by default; only expose it on a trusted network.
- No retention or delete yet. To start over, stop the daemon and remove `data/spans.jsonl`.
- The Network table renders every row without virtualization, so traces with many thousands of spans will feel slow.
- OTLP over gRPC and compressed OTLP/HTTP are not supported.
