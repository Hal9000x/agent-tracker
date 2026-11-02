// node --test integrations/claude-code-web/agent-tracker-hook.test.mjs

import { after, before, beforeEach, test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { appendFileSync, mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

const HOOK = join(import.meta.dirname, "agent-tracker-hook.mjs");

let server;
let url;
let received;
let status;
let dir;

before(async () => {
  server = createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      received.push({ path: req.url, type: req.headers["content-type"], auth: req.headers.authorization, event: JSON.parse(body) });
      res.writeHead(status, { "content-type": "application/json" }).end("{}");
    });
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  url = `http://127.0.0.1:${server.address().port}`;
});

after(() => {
  server.closeAllConnections();
  server.close();
});

beforeEach(() => {
  received = [];
  status = 200;
  dir = mkdtempSync(join(tmpdir(), "agent-tracker-hook-"));
});

/** Runs the hook as Claude Code would: the event on stdin, in a cloud session unless `remote` is false. */
function runHook(event, { remote = true, to = url, token = "" } = {}) {
  const env = { ...process.env, TMPDIR: dir, TEMP: dir, TMP: dir, CLAUDE_CODE_REMOTE: remote ? "true" : "", AGENT_TRACKER_TOKEN: token };
  const child = spawn(process.execPath, [HOOK, to], { env });
  let stdout = "";
  child.stdout.on("data", (c) => (stdout += c));
  child.stdin.end(JSON.stringify(event));
  return new Promise((resolve) => child.on("close", (code) => resolve({ code, stdout })));
}

const LINE = (n) => JSON.stringify({ type: "assistant", n }) + "\n";

test("sends each event with the transcript text appended since the last one", async () => {
  const transcript = join(dir, "sess.jsonl");
  writeFileSync(transcript, LINE(1) + LINE(2) + '{"type":"assis');
  const event = { session_id: "s", hook_event_name: "UserPromptSubmit", transcript_path: transcript, prompt: "hi" };

  assert.deepEqual(await runHook(event), { code: 0, stdout: "" });
  appendFileSync(transcript, 'tant","n":3}\n');
  await runHook({ ...event, hook_event_name: "Stop" });

  assert.equal(received.length, 2);
  assert.deepEqual(received.map((r) => [r.path, r.type]), [["/api/hooks/claude-code", "application/json"], ["/api/hooks/claude-code", "application/json"]]);
  assert.equal(received[0].event.prompt, "hi");
  assert.deepEqual(received[0].event.transcript_chunks, { main: { from: 0, text: LINE(1) + LINE(2) } }, "an unfinished line waits");
  const end = Buffer.byteLength(LINE(1) + LINE(2));
  assert.deepEqual(received[1].event.transcript_chunks, { main: { from: end, text: '{"type":"assistant","n":3}\n' } });
});

test("a subagent event also sends the subagent's transcript", async () => {
  const transcript = join(dir, "sess.jsonl");
  writeFileSync(transcript, LINE(1));
  mkdirSync(join(dir, "sess", "subagents"), { recursive: true });
  writeFileSync(join(dir, "sess", "subagents", "agent-a1.jsonl"), LINE(7));

  await runHook({ session_id: "s", hook_event_name: "SubagentStop", transcript_path: transcript, agent_id: "a1" });

  assert.deepEqual(received[0].event.transcript_chunks, { main: { from: 0, text: LINE(1) }, a1: { from: 0, text: LINE(7) } });
});

test("text the tracker didn't accept is sent again with the next event", async () => {
  const transcript = join(dir, "sess.jsonl");
  writeFileSync(transcript, LINE(1));
  const event = { session_id: "s", hook_event_name: "PreToolUse", transcript_path: transcript };
  status = 500;
  await runHook(event);
  status = 200;
  await runHook(event);

  assert.deepEqual(received.map((r) => r.event.transcript_chunks.main), [{ from: 0, text: LINE(1) }, { from: 0, text: LINE(1) }]);
});

test("sends AGENT_TRACKER_TOKEN when the environment sets it, and no token otherwise", async () => {
  const event = { session_id: "s", hook_event_name: "Stop", transcript_path: join(dir, "missing.jsonl") };
  await runHook(event);
  await runHook(event, { token: "tok-123" });

  assert.deepEqual(received.map((r) => r.auth), [undefined, "Bearer tok-123"]);
});

test("does nothing outside a cloud session, and never fails the hook", async () => {
  const event = { session_id: "s", hook_event_name: "Stop", transcript_path: join(dir, "missing.jsonl") };
  assert.deepEqual(await runHook(event, { remote: false }), { code: 0, stdout: "" });
  assert.equal(received.length, 0);
  assert.deepEqual(await runHook(event, { to: "http://127.0.0.1:9" }), { code: 0, stdout: "" }, "tracker unreachable");
});
