// Agent Tracker hook for Claude Code on the web. A cloud session can't reach your machine,
// but it can push to its own repository. So this commits each hook event, with the
// transcript text added since the last one, to the branch agent-tracker/<session id>,
// and your local tracker fetches those branches. It never touches the session's own
// branch, index or files. Local sessions skip it: they use the http hooks from
// `agent-tracker install`. `agent-tracker install-web` sets it up as
// .claude/agent-tracker-hook.mjs, run as an async command hook for each event.
//
// It prints nothing and always exits 0, so Claude Code carries on whatever happens here.

import { execFileSync } from "node:child_process";
import { appendFileSync, closeSync, fstatSync, mkdirSync, openSync, readdirSync, readFileSync, readSync, renameSync, rmdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const MAX_CHUNK = 8 << 20;
/** How long a hook waits for this session's other hook runs; longer than any push takes. */
const LOCK_WAIT_MS = 60_000;

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

/** Runs `fn` holding this session's lock, so its hook runs, which Claude Code starts in parallel, take turns. */
async function locked(lock, fn) {
  const until = Date.now() + LOCK_WAIT_MS;
  for (;;) {
    try {
      mkdirSync(lock);
      break;
    } catch {
      // A run that died leaves its lock behind.
      try {
        if (Date.now() - statSync(lock).mtimeMs > LOCK_WAIT_MS) rmdirSync(lock);
      } catch {}
      if (Date.now() > until) return;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  try {
    fn();
  } finally {
    rmdirSync(lock);
  }
}

function readState(path) {
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return { offsets: {} };
  }
}

/** Commits `spool` as events.jsonl on top of `parent` with plumbing, so HEAD, the index and the files stay as they are. */
function commit(git, spool, parent, count) {
  const blob = git(["hash-object", "-w", spool]);
  const tree = git(["mktree"], `100644 blob ${blob}\tevents.jsonl\n`);
  return git(["commit-tree", tree, ...(parent ? ["-p", parent] : []), "-m", `agent-tracker: ${count} event${count === 1 ? "" : "s"} [skip ci]`]);
}

async function main() {
  if (process.env.CLAUDE_CODE_REMOTE !== "true") return;
  const time = Date.now();
  const event = JSON.parse(readFileSync(0, "utf8"));
  const repo = process.env.CLAUDE_PROJECT_DIR || event.cwd;
  const session = String(event.session_id).replace(/[^\w-]/g, "_");
  const base = join(tmpdir(), `agent-tracker-${session}`);
  const env = { ...process.env, GIT_AUTHOR_NAME: "Agent Tracker", GIT_AUTHOR_EMAIL: "agent-tracker@localhost", GIT_COMMITTER_NAME: "Agent Tracker", GIT_COMMITTER_EMAIL: "agent-tracker@localhost", GIT_TERMINAL_PROMPT: "0" };
  const git = (args, input) => execFileSync("git", args, { cwd: repo, env, input, encoding: "utf8", timeout: 30_000, stdio: ["pipe", "pipe", "ignore"] }).trim();

  // Hooks start in the order things happen but take the lock in any order, so each one
  // queues its event at once, and whoever holds the lock sends everything queued, oldest first.
  const queue = `${base}.queue`;
  mkdirSync(queue, { recursive: true });
  const queued = join(queue, `${String(time).padStart(15, "0")}-${process.pid}`);
  writeFileSync(`${queued}.tmp`, JSON.stringify({ ...event, agent_tracker_time: time }));
  renameSync(`${queued}.tmp`, `${queued}.json`);

  await locked(`${base}.lock`, () => {
    const state = readState(`${base}.json`);
    const files = readdirSync(queue).filter((f) => f.endsWith(".json")).sort();
    for (const file of files) {
      const next = JSON.parse(readFileSync(join(queue, file), "utf8"));
      const chunks = {};
      for (const [key, path] of transcripts(next)) {
        const from = state.offsets[path] ?? 0;
        const [text, end] = readNew(path, from);
        if (text) {
          chunks[key] = { from, text };
          state.offsets[path] = end;
        }
      }
      appendFileSync(`${base}.jsonl`, JSON.stringify({ ...next, transcript_chunks: chunks }) + "\n");
      rmSync(join(queue, file));
    }
    if (files.length) state.head = commit(git, `${base}.jsonl`, state.head, files.length);
    try {
      // A failed push is retried by the next hook run, whose commit carries everything so far.
      if (state.head !== state.pushed) git(["push", "--quiet", "origin", `${state.head}:refs/heads/agent-tracker/${session}`]);
      state.pushed = state.head;
    } finally {
      writeFileSync(`${base}.json`, JSON.stringify(state));
    }
  });
}

main()
  .catch(() => {})
  .finally(() => process.exit(0));
