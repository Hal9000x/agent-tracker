import { useState } from "react";
import claudeCodeHooks from "../../../integrations/claude-code/settings.json?raw";
import { fmtAgo, type TraceSummary } from "../model";

interface Harness {
  name: string;
  /** Which trace sources this harness produces. */
  matches: (source: string) => boolean;
  what: string;
  where: string;
  snippet: string;
}

const DEFAULT_ORIGIN = "http://127.0.0.1:4318";

const HARNESSES: Harness[] = [
  {
    name: "Claude Code",
    matches: (s) => s === "claude-code",
    what: "Every prompt as a trace: tool calls, subagents, and the model calls read from the session transcript.",
    where: "Merge into ~/.claude/settings.json (all projects) or <project>/.claude/settings.local.json (one project). Running sessions pick it up without a restart.",
    snippet: claudeCodeHooks,
  },
  {
    name: "OpenTelemetry",
    matches: (s) => !s.startsWith("sdk:") && !["claude-code", "codex", "hermes"].includes(s),
    what: "Any agent framework that exports OTLP/HTTP traces (OpenAI Agents SDK, LangGraph via OpenLLMetry, Pydantic AI, …). GenAI semantic conventions are understood.",
    where: "Set in the environment of the process you want to trace.",
    snippet: `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=${DEFAULT_ORIGIN}/v1/traces\nOTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf`,
  },
  {
    name: "Your own agents",
    matches: (s) => s.startsWith("sdk:"),
    what: "Dependency-free clients for TypeScript (clients/typescript/tracker.ts) and Python (clients/python/agent_tracker.py).",
    where: "Wrap your agent's steps; spans nest by async context.",
    snippet: `const t = new Tracker({ url: "${DEFAULT_ORIGIN}" });\nawait t.trace("Plan a trip", prompt, () =>\n  t.agent("planner", { input: prompt }, () =>\n    t.llm("plan", { model: "claude-sonnet-5-5" }, callModel)));`,
  },
];

function lastSeen(traces: TraceSummary[], h: Harness): number | null {
  let latest: number | null = null;
  for (const t of traces) if (t.source && h.matches(t.source) && (latest === null || t.start > latest)) latest = t.start;
  return latest;
}

function Snippet({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  const copy = () =>
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    });
  return (
    <div className="snippet">
      <button className="tool-btn copy" onClick={copy}>
        {copied ? "Copied" : "Copy"}
      </button>
      <pre>{text}</pre>
    </div>
  );
}

/**
 * The address harnesses should send to. Keeps 127.0.0.1 for local pages: on Windows
 * "localhost" can resolve to ::1 first, where the daemon is not listening.
 */
function trackerOrigin(): string {
  const local = ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  return local ? `http://127.0.0.1:${location.port || "4318"}` : location.origin;
}

/** Setup instructions per harness, with whether data from it is arriving. */
export function ConnectGuide({ traces }: { traces: TraceSummary[] }) {
  // The integration files assume the default address; show the one this tracker actually runs on.
  const here = (s: string) => s.split(DEFAULT_ORIGIN).join(trackerOrigin());
  return (
    <div className="connect">
      {HARNESSES.map((h) => {
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
            <p className="muted">{h.where}</p>
            <Snippet text={here(h.snippet)} />
          </section>
        );
      })}
    </div>
  );
}
