import { useState } from "react";
import type { Session } from "../api";
import { fmtAgo, type TraceSummary } from "../model";

interface Harness {
  name: string;
  /** Which trace sources this harness produces. */
  matches: (source: string) => boolean;
  what: string;
  /** One paragraph, or numbered steps. */
  where: string | string[];
  snippets: { label?: string; text: string }[];
}

const DEFAULT_PORT = "4318";

const claudeCode = (s: string) => s === "claude-code";

/**
 * The address harnesses should send to. Keeps 127.0.0.1 for local pages: on Windows
 * "localhost" can resolve to ::1 first, where the daemon is not listening.
 */
function trackerOrigin(): string {
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  return local ? `http://127.0.0.1:${location.port || DEFAULT_PORT}` : location.origin;
}

const OTEL: Omit<Harness, "snippets"> = {
  name: "OpenTelemetry",
  matches: (s) => !s.startsWith("sdk:") && !["claude-code", "codex", "hermes"].includes(s),
  what: "Any agent framework that exports OTLP/HTTP traces (OpenAI Agents SDK, LangGraph via OpenLLMetry, Pydantic AI, …). GenAI semantic conventions are understood.",
  where: "Set in the environment of the process you want to trace.",
};

/** Harnesses for a tracker on this machine. */
function localHarnesses(origin: string): Harness[] {
  const port = new URL(origin).port || DEFAULT_PORT;
  const install = port === DEFAULT_PORT ? "agent-tracker install" : `agent-tracker install --port ${port}`;
  return [
    {
      name: "Claude Code",
      matches: claudeCode,
      what: "Every prompt as a trace: tool calls, subagents, and each model call with its reasoning and messages, read from the session transcript. Covers the terminal and the desktop app's Code tab.",
      where: "In a terminal, run the tracker program with install (on Windows, .\\agent-tracker.exe install). It adds hooks to ~/.claude/settings.json, keeping a backup; agent-tracker uninstall takes them out. Running sessions pick them up without a restart.",
      snippets: [{ text: install }],
    },
    {
      name: "Codex",
      matches: (s) => s === "codex",
      what: "Every turn of the Codex CLI or app as a trace: tool calls, subagents, and each model call with its reasoning summary and messages, read from the session's rollout file.",
      where: "The same install adds hooks to ~/.codex/hooks.json when Codex is set up. Then run codex, type /hooks and approve the new hooks once; Codex skips hooks nobody approved.",
      snippets: [{ text: install }],
    },
    {
      name: "Claude Code on the web",
      matches: () => false,
      what: "Sessions on claude.ai/code and the desktop app's cloud sessions, with model calls and reasoning.",
      where: "These run on Anthropic's machines, so they need a tracker they can reach over the internet: run Agent Tracker on a server with --public-url and a password (see Hosting in the README), then follow the steps in that tracker's Connect panel.",
      snippets: [],
    },
    { ...OTEL, snippets: [{ text: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=${origin}/v1/traces\nOTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf` }] },
    {
      name: "Your own agents",
      matches: (s) => s.startsWith("sdk:"),
      what: "Dependency-free clients for TypeScript (clients/typescript/tracker.ts) and Python (clients/python/agent_tracker.py).",
      where: "Wrap your agent's steps; spans nest by async context.",
      snippets: [
        {
          text: `const t = new Tracker({ url: "${origin}" });\nawait t.trace("Plan a trip", prompt, () =>\n  t.agent("planner", { input: prompt }, () =>\n    t.llm("plan", { model: "claude-sonnet-5-5" }, callModel)));`,
        },
      ],
    },
  ];
}

/** Harnesses for a tracker reachable from the internet, which hooks reach with the ingest token. */
function hostedHarnesses(origin: string, token: string): Harness[] {
  const host = new URL(origin).hostname;
  return [
    {
      name: "Claude Code on the web",
      matches: claudeCode,
      what: "Sessions on claude.ai/code and the desktop app's cloud sessions: tool calls, subagents, and each model call with its reasoning and messages, sent by a hook script committed to the repository.",
      where: [
        "In the repository, run the command below, then commit .claude/settings.json and .claude/agent-tracker-hook.mjs and push.",
        `At claude.ai/code, open your cloud environment's settings: set Network access to Custom and add ${host} to Allowed domains (keep the package-manager defaults if your setup needs them).`,
        `In the same environment, add a network secret for ${host}: type Bearer, header Authorization, holding the ingest token below. It is added on the way out, so the token never sits in the repository or the session.`,
      ],
      snippets: [{ text: `agent-tracker install-web ${origin}` }, { label: "Ingest token", text: token }],
    },
    {
      ...OTEL,
      snippets: [{ text: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=${origin}/v1/traces\nOTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf\nOTEL_EXPORTER_OTLP_TRACES_HEADERS=Authorization=Bearer ${token}` }],
    },
  ];
}

function lastSeen(traces: TraceSummary[], h: Harness): number | null {
  let latest: number | null = null;
  for (const t of traces) if (t.source && h.matches(t.source) && (latest === null || t.start > latest)) latest = t.start;
  return latest;
}

function Snippet({ label, text }: { label?: string; text: string }) {
  const [copied, setCopied] = useState(false);
  const copy = () =>
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  return (
    <div className="snippet">
      {label && <span className="snippet-label">{label}</span>}
      <button className="tool-btn copy" onClick={copy}>
        {copied ? "Copied" : "Copy"}
      </button>
      <pre>{text}</pre>
    </div>
  );
}

/** Setup instructions per harness, with whether data from it is arriving. */
export function ConnectGuide({ traces, session }: { traces: TraceSummary[]; session: Session }) {
  const origin = trackerOrigin();
  const harnesses = session.auth ? hostedHarnesses(origin, session.ingest_token ?? "") : localHarnesses(origin);
  return (
    <div className="connect">
      {harnesses.map((h) => {
        const seen = lastSeen(traces, h);
        return (
          <section key={h.name} className="harness">
            <div className="harness-head">
              <h3>{h.name}</h3>
              <span className={seen === null ? "pill" : "pill live"}>
                {seen === null ? "no data yet" : `receiving · last trace ${fmtAgo(Date.now() - seen)}`}
              </span>
            </div>
            <p>{h.what}</p>
            {typeof h.where === "string" ? (
              <p className="muted">{h.where}</p>
            ) : (
              <ol className="muted steps">
                {h.where.map((step) => (
                  <li key={step}>{step}</li>
                ))}
              </ol>
            )}
            {h.snippets.map((s) => (
              <Snippet key={s.text} {...s} />
            ))}
          </section>
        );
      })}
    </div>
  );
}
