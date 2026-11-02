# Agent Tracker

Agent Tracker records every step an AI agent takes after you send it a message, then lets you inspect the run like a browser's Network panel or like an n8n/ComfyUI node graph.

- **Network view**: a historic log of spans with Name, Status, Type, Initiator, Size, Time and Waterfall columns. It has an overview timeline you can drag to clip a time range, type chips and a `key:value` filter, and a details pane with the Headers, Payload, Preview, Response, Initiator and Timing tabs.
- **Graph view**: the run drawn as a flow. A single agent reads as a straight chain. A swarm shows splits, forks, joins, parallel branches and messages between agents. Each agent's steps sit inside a group box. Click any node to light up everything upstream and downstream of it. Switch to **Agents** to fold steps into one node per agent and see how the agents interacted.

Data arrives live over Server-Sent Events, so runs appear while they are still going.

## Quick start

Download the build for your system from the repository's Releases page; [TESTERS.md](TESTERS.md) walks through it. To build it yourself instead, you need Rust 1.88+ and Node 22.18+ (Node 24 recommended). One command builds the UI and the server:

```bash
npm run build
```

That produces a single file, `daemon/target/release/agent-tracker.exe` (`agent-tracker` on macOS and Linux), with the UI built in. Copy it anywhere and double-click it, or run it from a terminal:

- It starts on http://127.0.0.1:4318 and opens that page in your browser.
- Click **Play demo runs** (or **▶ Demo** in the top bar) to replay three sample runs live: a single agent with a tool call, a research swarm (split, three parallel researchers, join, a fork into reviewers with a message between them), and a support handoff with a failing tool.
- Starting it again while it runs just opens the browser on the running copy.
- Traces are saved in a per-user folder, printed at startup: `%LOCALAPPDATA%\agent-tracker` on Windows, `~/Library/Application Support/agent-tracker` on macOS, `~/.local/share/agent-tracker` on Linux.

Flags: `--host` (default `127.0.0.1`), `--port` (default `4318`), `--data` (default: the per-user folder), `--no-open` (don't open a browser), `--public-url` (see [Hosting](#hosting)). `agent-tracker --version` prints the version.

## Sending data

There are four ways in. They all produce the same spans.

### Claude Code

Run `agent-tracker install`. It adds the `http` hooks in [`integrations/claude-code/settings.json`](integrations/claude-code/settings.json) to `~/.claude/settings.json`, keeping your other settings and a backup of the old file (`settings.json.bak-agent-tracker`). It also sets up Codex when `~/.codex` exists. Running it again replaces the hooks (pass `--port` if the tracker isn't on 4318), and `agent-tracker uninstall` takes them out. To scope tracking to one project instead, merge that file into its `.claude/settings.local.json` by hand.

Each prompt you submit becomes one trace: the prompt, the main agent, every tool call, and each subagent nested under the `Agent` tool call that started it. Parallel subagents show up as a fork. If the tracker is not running, Claude Code prints a non-blocking "hook failed" notice and carries on.

Model calls come from the session transcript that every hook event names (`~/.claude/projects/<project>/<session>.jsonl`). Each API response becomes an `llm` step with its model, token counts (input includes cache reads and writes) and timing. Under it, each thinking block becomes a `reasoning` step and each block of reply text a `message` step. The tracker reads only `.jsonl` files under `~/.claude/projects`, and only for turns it has seen, so starting it mid-session doesn't fill the list with earlier turns.

A message you send while Claude is still working starts a new trace, and the previous turn ends normally rather than as interrupted.

### Codex

`agent-tracker install` adds [`integrations/codex/hooks.json`](integrations/codex/hooks.json) to `~/.codex/hooks.json`. Then run `codex`, type `/hooks` and approve the new hooks once: Codex silently skips hooks nobody approved. The hooks pipe each event to the tracker with `curl` (`curl.exe` on Windows, where Codex runs hook commands through PowerShell); with the tracker down, each costs about 0.25 s. For reasoning text, set `model_reasoning_summary = "detailed"` in `~/.codex/config.toml`.

Each turn of the Codex CLI or app becomes one trace: the prompt, the main agent, tool calls, and subagents nested under the `spawn_agent` call that started them. Model calls come from the session's rollout file under `~/.codex/sessions`: one `llm` step per response with its token counts, a `reasoning` step per reasoning item and a `message` step per reply. Codex keeps the full reasoning encrypted, so a reasoning step shows the summary Codex writes, or "(reasoning not shown)" when there is none.

### Claude Code on the web

Cloud sessions (claude.ai/code, and the desktop app's cloud sessions) run on Anthropic's machines. They load hooks only from the repository's committed `.claude/settings.json`. With the default network settings, the one place they can send data is their own GitHub repository. So the hook sends the events through the repository, and nothing needs setting up on claude.ai.

In your clone of the repository, run `agent-tracker install-web`, then commit and push the two files it writes:

- `.claude/settings.json` gets async command hooks;
- [`.claude/agent-tracker-hook.mjs`](integrations/claude-code-web/agent-tracker-hook.mjs) goes beside it.

In a cloud session, the script commits each event, with the transcript text added since the last one, to the branch `agent-tracker/<session id>` and pushes it. It uses git plumbing, so the session's own branch, index and files stay as they were, and `[skip ci]` keeps the pushes from starting CI. In local sessions (`CLAUDE_CODE_REMOTE` isn't set) it does nothing, so local tracking isn't doubled.

`install-web` also registers the clone with the tracker. While the tracker runs, it fetches `agent-tracker/*` branches into the clone every 15 seconds, using your usual git credentials and its own ref namespace, `refs/agent-tracker/`. It then reads the events it hasn't seen yet, timed from when they happened in the session. Web sessions appear under the source `claude-code-web`, usually within about 20 seconds of each step.

Anyone who can read the repository can read those branches, which hold prompts, tool output and reasoning. Delete them whenever you like. `agent-tracker uninstall-web` takes the hooks and script out and stops fetching. claude.ai chat and the desktop app's Chat tab have no hooks and can't be tracked.

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

Other endpoints: `GET /api/traces`, `GET /api/traces/{id}`, `DELETE /api/traces/{id}`, `GET /api/stream` (SSE), `POST /v1/traces` (OTLP), `POST /api/hooks/claude-code`, `POST /api/hooks/codex`, `POST /api/demo` (replays the demo runs), and for [hosting](#hosting) `POST /api/login`, `POST /api/logout`, `GET /api/session`.

## Hosting

A hosted tracker is this same program, run on a server you control so that agents on other machines can report to it over OTLP or the clients. Claude Code on the web doesn't need one. Start it with `--public-url`, the address people and agents use. That turns on sign-in. A `--host` other than this machine also requires `--public-url`.

- **Signing in:** the owner signs in with `AGENT_TRACKER_USERNAME` (default `admin`) and `AGENT_TRACKER_PASSWORD` (12+ characters), both read from the environment. It refuses to start without a password. Sessions last 30 days. After ten failed sign-ins in a minute, that client is refused for the rest of the minute; others aren't. Behind a proxy on the same machine or a private network, the client is the address the proxy puts last in `X-Forwarded-For`.
- **Sending data:** hooks, `/api/spans` and `/v1/traces` need `Authorization: Bearer <ingest token>`. The token is made on first start, kept in `<data>/ingest-token`, and shown in the Connect panel once you're signed in. To replace it, delete that file and restart. Whoever holds the token can add data; only the owner can read it.
- **HTTPS:** the program speaks plain HTTP, so put HTTPS in front of it. Use a reverse proxy, for example `caddy reverse-proxy --from tracker.example.com --to 127.0.0.1:4318`, or your platform's.
- A hosted tracker answers only to its public host name and the local names, and it never opens transcript files on the server it runs on.

With Docker ([`Dockerfile`](Dockerfile)):

```bash
docker build -t agent-tracker .
docker run -d -p 127.0.0.1:4318:4318 -v agent-tracker-data:/data \
  -e AGENT_TRACKER_PASSWORD='choose 12+ characters' agent-tracker --public-url https://tracker.example.com
```

Without Docker, run the Linux build with the same flags (`--host 127.0.0.1 --public-url https://...` behind a proxy on the same machine).

## Using the views

**Filter** (Network view, and Highlight in the Graph view): plain words match names, agents and payloads. `agent:`, `kind:`, `status:`, `model:` and `name:` match one field, and `-` excludes. For example, `agent:researcher -status:ok`. Ctrl-click type chips to combine them.

**Clip**: drag across the overview timeline. Drag the edges to resize the clip, drag inside it to move it, and double-click to clear it. The table rescales to the clip, and the Graph view dims spans outside it. **Export** downloads the visible spans as JSON.

**How the graph is drawn**: a span leads into its children. Children that overlap in time run in parallel, and children that follow one another chain in sequence. Children of a `split` or `fork` are always parallel. The last steps of one stage feed the next. In the Agents view, an edge back to an agent that contains the source is drawn as an arc labelled *returns*, and it is left out of the click-to-trace highlight.

## Layout

```
daemon/        Rust server (axum): ingest, store, SSE, OTLP, Claude Code and Codex adapters, sign-in, hook install, embedded UI
web/           React + TypeScript UI (Vite, React Flow, dagre)
clients/       TypeScript and Python clients, with demos
integrations/  Hook settings for Claude Code, Codex and Claude Code on the web
```

Spans live in memory and are appended to `spans.jsonl` in the data folder. The log is replayed on startup.

Pushing a `v*` tag runs [the release workflow](.github/workflows/release.yml): it tests and builds on Windows, macOS (one binary for Apple silicon and Intel) and Linux (statically linked), and publishes a pre-release with [TESTERS.md](TESTERS.md) in each download.

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

- A tracker on your machine has no sign-in; it binds to `127.0.0.1` by default. To keep web pages you visit from reaching it, it refuses requests that name a foreign `Host` (DNS rebinding) and writes that aren't `application/json` or `application/x-protobuf` (cross-site form posts). A [hosted](#hosting) tracker has one owner account; there are no per-user accounts.
- No automatic retention. Delete traces from the list; a deleted trace stays in `data/spans.jsonl` (with a tombstone) until you remove the file.
- Prompts, tool inputs and outputs, reasoning and replies are stored in plain text in `data/spans.jsonl`. They never leave your machine, but treat the file as you would the transcripts themselves.
- Claude Code transcripts and Codex rollout files are undocumented formats. The readers skip what they don't recognize, so a format change loses model calls rather than breaking tracking.
- The tracker keeps which trace each running subagent belongs to in memory. After a restart, a subagent that was already running joins the current turn.
- OTLP over gRPC and compressed OTLP/HTTP are not supported.
