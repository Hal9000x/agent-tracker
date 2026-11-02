# Agent Tracker

Agent Tracker records every step an AI agent takes after you send it a message, then lets you inspect the run like a browser's Network panel or like an n8n/ComfyUI node graph.

- **Network view**: a historic log of spans with Name, Status, Type, Initiator, Size, Time and Waterfall columns. It has an overview timeline you can drag to clip a time range, type chips and a `key:value` filter, and a details pane with the Headers, Payload, Preview, Response, Initiator and Timing tabs.
- **Graph view**: the run drawn as a flow. A single agent reads as a straight chain. A swarm shows splits, forks, joins, parallel branches and messages between agents. Each agent's steps sit inside a group box. Click any node to light up everything upstream and downstream of it. Switch to **Agents** to fold steps into one node per agent and see how the agents interacted.

Data arrives live over Server-Sent Events, so runs appear while they are still going.

## Quick start

Building requires Rust 1.88+ and Node 22.18+ (Node 24 recommended). One command builds the UI and the server:

```bash
npm run build
```

That produces a single file, `daemon/target/release/agent-tracker.exe` (`agent-tracker` on macOS and Linux), with the UI built in. Copy it anywhere and double-click it, or run it from a terminal:

- It starts on http://127.0.0.1:4318 and opens that page in your browser.
- Click **Play demo runs** (or **▶ Demo** in the top bar) to replay three sample runs live: a single agent with a tool call, a research swarm (split, three parallel researchers, join, a fork into reviewers with a message between them), and a support handoff with a failing tool.
- Starting it again while it runs just opens the browser on the running copy.
- Traces are saved in a `data` folder next to where it was started.

Flags: `--host` (default `127.0.0.1`), `--port` (default `4318`), `--data` (default `./data`), `--no-open` (don't open a browser).

## Sending data

There are four ways in. They all produce the same spans.

### Claude Code

Merge [`integrations/claude-code/settings.json`](integrations/claude-code/settings.json) into a Claude Code settings file (`~/.claude/settings.json` for every project, or `.claude/settings.local.json` for one). It adds `http` hooks that post each event to the tracker.

Each prompt you submit becomes one trace: the prompt, the main agent, every tool call, and each subagent nested under the `Agent` tool call that started it. Parallel subagents show up as a fork. If the tracker is not running, Claude Code prints a non-blocking "hook failed" notice and carries on.

Model calls come from the session transcript that every hook event names (`~/.claude/projects/<project>/<session>.jsonl`). Each API response becomes an `llm` step with its model, token counts (input includes cache reads and writes) and timing. Under it, each thinking block becomes a `reasoning` step and each block of reply text a `message` step. The tracker reads only `.jsonl` files under `~/.claude/projects`, and only for turns it has seen, so starting it mid-session doesn't fill the list with earlier turns.

A message you send while Claude is still working starts a new trace, and the previous turn ends normally rather than as interrupted.

### Codex

Save [`integrations/codex/hooks.json`](integrations/codex/hooks.json) as `~/.codex/hooks.json` (merge the event arrays if you already have one). Then run `codex`, type `/hooks` and approve the new hooks once: Codex silently skips hooks nobody approved. The hooks pipe each event to the tracker with `curl` (`curl.exe` on Windows); with the tracker down, each costs about 0.25 s.

Each turn of the Codex CLI or app becomes one trace: the prompt, the main agent, tool calls, and subagents nested under the `spawn_agent` call that started them. Model calls come from the session's rollout file under `~/.codex/sessions`: one `llm` step per response with its token counts, a `reasoning` step per reasoning item and a `message` step per reply. Codex keeps the full reasoning encrypted, so a reasoning step shows the summary Codex writes, or "(reasoning not shown)" when there is none.

### OpenTelemetry

Point any OTLP/HTTP exporter at the tracker. Protobuf and JSON both work.

```bash
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces
```

Spans are classified with the [GenAI semantic conventions](https://opentelemetry.io/docs/specs/semconv/gen-ai/) (`gen_ai.operation.name`: `invoke_agent`, `chat`, `execute_tool`, …), falling back to OpenInference's `openinference.span.kind`. Model, token usage, input/output messages, errors and span links are picked up. Compressed payloads are rejected; set `OTEL_EXPORTER_OTLP_COMPRESSION=none`.

### TypeScript

[`clients/typescript/tracker.ts`](clients/typescript/tracker.ts) is one dependency-free file. [`examples/demo.ts`](clients/typescript/examples/demo.ts) is the source of the built-in demo runs (`node clients/typescript/examples/demo.ts`, `DEMO_SPEED=4` to speed it up; [`scripts/record-demo.mjs`](scripts/record-demo.mjs) turns a run into the recording the server replays). Spans nest by async context, so `Promise.all` inside a `fork` or `split` gives parallel branches.

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

`POST /api/spans` takes one span update or an array of them, sent as `application/json`. Updates with the same `trace_id` + `span_id` merge, so you can report a span when it starts and again when it ends, in either order.

| Field | Type | Notes |
|---|---|---|
| `trace_id`, `span_id` | string | required |
| `parent_id` | string | the span that started this one |
| `kind` | `message` `agent` `llm` `tool` `split` `fork` `join` `reasoning` `step` | children of `split`/`fork` are drawn as parallel branches; unknown kinds become `step` |
| `name`, `agent` | string | `agent` is inherited by descendants for display |
| `start`, `end` | number | epoch milliseconds; a span without `end` is running |
| `status` | `running` `ok` `error` | defaults from `end`; a late `running` never reopens an ended span |
| `error` | string | |
| `input`, `output` | any JSON | |
| `model`, `tokens_in`, `tokens_out` | | |
| `attrs` | object | merged key by key |
| `links` | string[] | span ids this span received output from; drawn as dashed message edges |

Other endpoints: `GET /api/traces`, `GET /api/traces/{id}`, `DELETE /api/traces/{id}`, `GET /api/stream` (SSE), `POST /v1/traces` (OTLP), `POST /api/hooks/claude-code`, `POST /api/hooks/codex`, `POST /api/demo` (replays the demo runs).

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

Runs the server, UI and TypeScript client tests:

```bash
npm test
```

The Python client has its own:

```bash
python -m unittest clients/python/test_agent_tracker.py
```

For UI work, run the daemon, then `npm run dev` in `web/`. Vite proxies `/api` and `/v1` to the daemon on port 4318 (override with `AGENT_TRACKER_URL`).

## Limits

- No authentication. The daemon binds to `127.0.0.1` by default; only expose it on a trusted network. To keep web pages you visit from reaching it, it refuses requests that name a foreign `Host` (DNS rebinding) and writes that aren't `application/json` or `application/x-protobuf` (cross-site form posts).
- No automatic retention. Delete traces from the list; a deleted trace stays in `data/spans.jsonl` (with a tombstone) until you remove the file.
- Prompts, tool inputs and outputs, reasoning and replies are stored in plain text in `data/spans.jsonl`. They never leave your machine, but treat the file as you would the transcripts themselves.
- Claude Code transcripts and Codex rollout files are undocumented formats. The readers skip what they don't recognize, so a format change loses model calls rather than breaking tracking.
- The tracker keeps which trace each running subagent belongs to in memory. After a restart, a subagent that was already running joins the current turn.
- OTLP over gRPC and compressed OTLP/HTTP are not supported.
