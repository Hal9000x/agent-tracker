// Agent Tracker hook for Claude Code on the web. Cloud sessions run on Anthropic's machines,
// where the tracker can't read the session transcript, so this posts each hook event together
// with the transcript text appended since the last event. Local sessions skip it: they use the
// http hooks from `agent-tracker install`. Commit it as .claude/agent-tracker-hook.mjs and run it
// from .claude/settings.json:
//
//   "command": "node \"$CLAUDE_PROJECT_DIR/.claude/agent-tracker-hook.mjs\" https://tracker.example.com"
//
// It prints nothing and always exits 0, so Claude Code carries on whatever happens here.

import { closeSync, fstatSync, openSync, readFileSync, readSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const MAX_CHUNK = 8 << 20;

/** The text from byte `from` to the end of the last complete line, and the offset after it. */
function readNew(path, from) {
  let fd;
  try {
    fd = openSync(path, "r");
  } catch {
    return ["", from];
  }
  try {
    const buf = Buffer.alloc(Math.min(Math.max(fstatSync(fd).size - from, 0), MAX_CHUNK));
    const n = readSync(fd, buf, 0, buf.length, from);
    const end = buf.subarray(0, n).lastIndexOf(10) + 1;
    return [buf.subarray(0, end).toString("utf8"), from + end];
  } finally {
    closeSync(fd);
  }
}

/** [key, path] of each transcript the event names: `main`, and the subagent's under its id. */
function transcripts(event) {
  const main = event.transcript_path;
  if (typeof main !== "string") return [];
  const files = [["main", main]];
  if (event.agent_id) {
    files.push([event.agent_id, event.agent_transcript_path ?? join(main.replace(/\.jsonl$/, ""), "subagents", `agent-${event.agent_id}.jsonl`)]);
  }
  return files;
}

async function main() {
  const tracker = process.argv[2];
  if (process.env.CLAUDE_CODE_REMOTE !== "true" || !tracker) return;
  const event = JSON.parse(readFileSync(0, "utf8"));
  // How far each transcript has been sent, kept between this session's hook runs.
  const state = join(tmpdir(), `agent-tracker-${String(event.session_id).replace(/[^\w-]/g, "_")}.json`);
  let sent = {};
  try {
    sent = JSON.parse(readFileSync(state, "utf8"));
  } catch {}
  const chunks = {};
  const next = { ...sent };
  for (const [key, path] of transcripts(event)) {
    const from = sent[path] ?? 0;
    const [text, end] = readNew(path, from);
    if (text) {
      chunks[key] = { from, text };
      next[path] = end;
    }
  }
  // The environment's network secret normally adds the ingest token on the way out;
  // AGENT_TRACKER_TOKEN is for environments without one.
  const token = process.env.AGENT_TRACKER_TOKEN;
  const res = await fetch(new URL("api/hooks/claude-code", tracker.replace(/\/*$/, "/")), {
    method: "POST",
    headers: { "content-type": "application/json", ...(token && { authorization: `Bearer ${token}` }) },
    body: JSON.stringify({ ...event, transcript_chunks: chunks }),
    signal: AbortSignal.timeout(5000),
  });
  // Text the tracker didn't take goes again next time; it skips what it already has.
  if (res.ok) writeFileSync(state, JSON.stringify(next));
}

main()
  .catch(() => {})
  .finally(() => process.exit(0));
