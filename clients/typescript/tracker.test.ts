// node --test clients/typescript/tracker.test.ts

import { after, before, test } from "node:test";
import assert from "node:assert/strict";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { Tracker } from "./tracker.ts";

type Update = Record<string, unknown>;

const updates: Update[] = [];
let server: Server;
let t: Tracker;

before(async () => {
  server = createServer((req, res) => {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      if (req.method === "POST" && req.url === "/api/spans") updates.push(...[JSON.parse(body)].flat());
      res.end();
    });
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  t = new Tracker({ url: `http://127.0.0.1:${(server.address() as AddressInfo).port}` });
});

after(() => {
  server.closeAllConnections();
  server.close();
});

const startOf = (id: string) => updates.find((u) => u.span_id === id && "start" in u);
const endOf = (id: string) => updates.find((u) => u.span_id === id && "end" in u);

test("a BigInt output is sent as a string and the span still ends", async () => {
  let id = "";
  const result = await t.tool("count", {}, (span) => {
    id = span.id;
    return { rows: 42n };
  });
  assert.deepEqual(result, { rows: 42n });
  await t.flush();
  assert.equal(endOf(id)?.status, "ok");
  assert.deepEqual(endOf(id)?.output, { rows: "42" });
});

test("a circular input does not throw, the body runs, and the span still ends", async () => {
  const circular: Record<string, unknown> = { name: "loop" };
  circular.self = circular;
  let id = "";
  await t.tool("walk", { input: circular }, (span) => {
    id = span.id;
    return "done";
  });
  assert.notEqual(id, "", "body did not run");
  await t.flush();
  assert.deepEqual(startOf(id)?.input, { name: "loop", self: "[Circular]" });
  assert.equal(endOf(id)?.status, "ok");
});

test("trace -> agent -> tool links each span to its parent", async () => {
  const ids = { message: "", agent: "", tool: "" };
  await t.trace("nested", "q", (message) => {
    ids.message = message.id;
    return t.agent("planner", {}, (agent) => {
      ids.agent = agent.id;
      return t.tool("search", { input: "q" }, (tool) => {
        ids.tool = tool.id;
        return "hit";
      });
    });
  });
  await t.flush();
  assert.equal(startOf(ids.message)?.parent_id, undefined);
  assert.equal(startOf(ids.agent)?.parent_id, ids.message);
  assert.equal(startOf(ids.tool)?.parent_id, ids.agent);
  const spans = Object.values(ids);
  assert.equal(new Set(spans.map((id) => startOf(id)?.trace_id)).size, 1);
  for (const id of spans) assert.equal(endOf(id)?.status, "ok");
});
