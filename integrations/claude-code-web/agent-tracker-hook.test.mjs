// node --test integrations/claude-code-web/agent-tracker-hook.test.mjs

import { beforeEach, test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { appendFileSync, mkdirSync, mkdtempSync, renameSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const HOOK = join(import.meta.dirname, "agent-tracker-hook.mjs");

let dir;
let origin;
let clone;
let state;

const git = (cwd, ...args) => execFileSync("git", args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "ignore"] }).trim();

beforeEach(() => {
  // A cloud session's checkout: a clone of the repository, on its own branch.
  dir = mkdtempSync(join(tmpdir(), "agent-tracker-hook-"));
  origin = join(dir, "origin.git");
  clone = join(dir, "clone");
  state = join(dir, "state");
  mkdirSync(state);
  git(dir, "init", "-q", "--bare", origin);
  git(dir, "clone", "-q", origin, clone);
  writeFileSync(join(clone, "notes.txt"), "hello\n");
  git(clone, "add", ".");
  git(clone, "-c", "user.name=t", "-c", "user.email=t@example.com", "commit", "-q", "-m", "start");
  git(clone, "push", "-q", "origin", "HEAD:refs/heads/main");
});

/** Runs the hook as Claude Code would: the event on stdin, in a cloud session unless `remote` is false. */
function runHook(event, { remote = true, project = clone } = {}) {
  const env = { ...process.env, TMPDIR: state, TEMP: state, TMP: state, CLAUDE_CODE_REMOTE: remote ? "true" : "", CLAUDE_PROJECT_DIR: project };
  const child = spawn(process.execPath, [HOOK], { env });
  let stdout = "";
  child.stdout.on("data", (c) => (stdout += c));
  child.stdin.end(JSON.stringify(event));
  return new Promise((resolve) => child.on("close", (code) => resolve({ code, stdout })));
}

/** The events on the session's branch in the repository it pushes to. */
function pushed(session) {
  try {
    return git(origin, "show", `refs/heads/agent-tracker/${session}:events.jsonl`).split("\n").map((l) => JSON.parse(l));
  } catch {
    return [];
  }
}

const LINE = (n) => JSON.stringify({ type: "assistant", n }) + "\n";

test("commits each event, with the transcript text added since the last one, to the session's branch", async () => {
  const transcript = join(dir, "sess.jsonl");
  writeFileSync(transcript, LINE(1) + LINE(2) + '{"type":"assis');
  const event = { session_id: "s1", hook_event_name: "UserPromptSubmit", transcript_path: transcript, prompt: "hi" };
  const head = git(clone, "rev-parse", "HEAD");

  assert.deepEqual(await runHook(event), { code: 0, stdout: "" });
  appendFileSync(transcript, 'tant","n":3}\n');
  await runHook({ ...event, hook_event_name: "Stop" });

  const events = pushed("s1");
  assert.deepEqual(events.map((e) => [e.hook_event_name, e.prompt]), [["UserPromptSubmit", "hi"], ["Stop", "hi"]]);
  assert.deepEqual(events[0].transcript_chunks, { main: { from: 0, text: LINE(1) + LINE(2) } }, "an unfinished line waits");
  assert.deepEqual(events[1].transcript_chunks, { main: { from: Buffer.byteLength(LINE(1) + LINE(2)), text: '{"type":"assistant","n":3}\n' } });
  assert.ok(events.every((e) => Math.abs(e.agent_tracker_time - Date.now()) < 60_000), "each event keeps when it happened");

  assert.match(git(origin, "log", "-1", "--format=%s", "agent-tracker/s1"), /\[skip ci\]/, "pushes don't start CI");
  assert.equal(git(clone, "rev-parse", "HEAD"), head, "the session's own branch is untouched");
  assert.equal(git(clone, "status", "--porcelain"), "", "and so are its index and files");
});

test("a subagent event also sends the subagent's transcript", async () => {
  const transcript = join(dir, "sess.jsonl");
  writeFileSync(transcript, LINE(1));
  mkdirSync(join(dir, "sess", "subagents"), { recursive: true });
  writeFileSync(join(dir, "sess", "subagents", "agent-a1.jsonl"), LINE(7));

  await runHook({ session_id: "s1", hook_event_name: "SubagentStop", transcript_path: transcript, agent_id: "a1" });

  assert.deepEqual(pushed("s1")[0].transcript_chunks, { main: { from: 0, text: LINE(1) }, a1: { from: 0, text: LINE(7) } });
});

test("hooks that run at the same time all get pushed, and send the transcript once", async () => {
  const transcript = join(dir, "sess.jsonl");
  writeFileSync(transcript, LINE(1));
  const ids = ["t1", "t2", "t3", "t4", "t5"];
  await Promise.all(ids.map((id) => runHook({ session_id: "s1", hook_event_name: "PreToolUse", tool_use_id: id, transcript_path: transcript })));

  const events = pushed("s1");
  assert.deepEqual(events.map((e) => e.tool_use_id).sort(), ids);
  assert.equal(events.filter((e) => e.transcript_chunks.main).length, 1, "each run takes up where the last one stopped");
});

test("events that couldn't be pushed go out with the next push", async () => {
  renameSync(origin, origin + ".away");
  assert.deepEqual(await runHook({ session_id: "s1", hook_event_name: "PreToolUse", tool_use_id: "t1" }), { code: 0, stdout: "" });
  renameSync(origin + ".away", origin);
  await runHook({ session_id: "s1", hook_event_name: "PostToolUse", tool_use_id: "t1" });

  assert.deepEqual(pushed("s1").map((e) => e.hook_event_name), ["PreToolUse", "PostToolUse"]);
});

test("does nothing outside a cloud session, and never fails the hook", async () => {
  const event = { session_id: "s1", hook_event_name: "Stop", transcript_path: join(dir, "missing.jsonl") };
  assert.deepEqual(await runHook(event, { remote: false }), { code: 0, stdout: "" });
  assert.deepEqual(pushed("s1"), []);
  const notARepo = mkdtempSync(join(tmpdir(), "agent-tracker-hook-norepo-"));
  assert.deepEqual(await runHook(event, { project: notARepo }), { code: 0, stdout: "" });
});
