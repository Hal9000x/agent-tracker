import { describe, expect, it } from "vitest";
import { agentGraph, agentOf, buildTree, compileFilter, flowEdges, flowRanks, fmtAgo, lineage, niceStep, packLanes, selfTime, type Kind, type Span } from "./model";

let seq = 0;
function sp(span_id: string, parent_id: string | undefined, kind: Kind, start: number, end: number | undefined, extra: Partial<Span> = {}): Span {
  return { trace_id: "t", span_id, parent_id, kind, name: span_id, start, end, status: end === undefined ? "running" : "ok", seq: ++seq, ...extra };
}

const pairs = (spans: Span[]) =>
  flowEdges(buildTree(spans))
    .map((e) => `${e.source}>${e.target}${e.link ? "~" : ""}`)
    .sort();

describe("flowEdges", () => {
  it("draws a single agent as a straight chain", () => {
    const spans = [
      sp("msg", undefined, "message", 0, 0),
      sp("agent", "msg", "agent", 1, 30, { agent: "assistant" }),
      sp("plan", "agent", "llm", 2, 10),
      sp("tool", "agent", "tool", 11, 18),
      sp("answer", "agent", "llm", 19, 29),
    ];
    expect(pairs(spans)).toEqual(["agent>plan", "msg>agent", "plan>tool", "tool>answer"].sort());
  });

  it("runs split children in parallel and feeds their last steps into the join", () => {
    const spans = [
      sp("orch", undefined, "agent", 0, 100),
      sp("split", "orch", "split", 1, 50),
      // Sequential in time, but children of a split are still branches.
      sp("r1", "split", "agent", 2, 10),
      sp("r1.search", "r1", "tool", 3, 9),
      sp("r2", "split", "agent", 11, 40),
      sp("r2.search", "r2", "tool", 12, 20),
      sp("r2.sum", "r2", "llm", 21, 39),
      sp("join", "orch", "join", 51, 52),
    ];
    expect(pairs(spans)).toEqual(
      ["orch>split", "split>r1", "split>r2", "r1>r1.search", "r2>r2.search", "r2.search>r2.sum", "r1.search>join", "r2.sum>join"].sort(),
    );
  });

  it("treats overlapping siblings as an implicit fork", () => {
    const spans = [
      sp("main", undefined, "agent", 0, 100),
      sp("a", "main", "tool", 1, 50),
      sp("b", "main", "tool", 2, 40),
      sp("c", "main", "tool", 60, 70),
    ];
    expect(pairs(spans)).toEqual(["main>a", "main>b", "a>c", "b>c"].sort());
  });

  it("keeps siblings sequential behind a long background sibling", () => {
    const spans = [sp("p", undefined, "agent", 0, 100), sp("a", "p", "tool", 1, 90), sp("b", "p", "tool", 10, 20), sp("c", "p", "tool", 30, 40)];
    expect(pairs(spans)).toEqual(["p>a", "p>b", "b>c"].sort());
  });

  it("feeds a successor from a sibling's exit only once that exit has ended", () => {
    // The Agent tool T returns at once; its background subagent A keeps working (G) while R runs.
    const spans = [
      sp("main", undefined, "agent", 0, 20),
      sp("T", "main", "tool", 0, 2),
      sp("A", "T", "agent", 2.5, 12),
      sp("G", "A", "tool", 3, 12),
      sp("R", "main", "tool", 5, 6),
    ];
    expect(pairs(spans)).toEqual(["main>T", "T>A", "A>G", "T>R"].sort());
  });

  it("keeps a running span's wave open", () => {
    const spans = [sp("main", undefined, "agent", 0, undefined), sp("a", "main", "tool", 1, undefined), sp("b", "main", "tool", 5, undefined)];
    expect(pairs(spans)).toEqual(["main>a", "main>b"]);
  });

  it("adds links as message edges and marks a duplicate flow edge", () => {
    const spans = [
      sp("fork", undefined, "fork", 0, 100),
      sp("checker", "fork", "agent", 1, 50),
      sp("verify", "checker", "tool", 2, 10),
      sp("critic", "fork", "agent", 1, 60),
      sp("critique", "critic", "llm", 11, 59, { links: ["verify", "missing"] }),
      sp("x", undefined, "step", 200, 201, { links: ["fork"] }),
    ];
    const got = pairs(spans);
    expect(got).toContain("verify>critique~");
    expect(got).toContain("fork>x~");
    expect(got.some((e) => e.includes("missing"))).toBe(false);
  });

  it("survives parent cycles in bad data", () => {
    const spans = [sp("a", "b", "step", 0, 1), sp("b", "a", "step", 0, 1), sp("ok", undefined, "step", 0, 1)];
    const tree = buildTree(spans);
    expect(flowEdges(tree)).toEqual([]);
    expect(agentOf(spans[0], tree)).toBeUndefined();
  });
});

describe("agentGraph", () => {
  const swarm = [
    sp("msg", undefined, "message", 0, 0),
    sp("orch", "msg", "agent", 1, 200, { agent: "orchestrator" }),
    sp("plan", "orch", "llm", 2, 10),
    sp("split", "orch", "split", 11, 60),
    sp("r1", "split", "agent", 12, 50, { agent: "r1" }),
    sp("r1.s", "r1", "tool", 13, 49),
    sp("r2", "split", "agent", 12, 55, { agent: "r2" }),
    sp("r2.s", "r2", "tool", 13, 54),
    sp("join", "orch", "join", 61, 62),
    sp("fork", "orch", "fork", 63, 120),
    sp("checker", "fork", "agent", 64, 100, { agent: "checker" }),
    sp("verify", "checker", "tool", 65, 70),
    sp("critic", "fork", "agent", 64, 110, { agent: "critic" }),
    sp("critique", "critic", "llm", 71, 109, { links: ["verify"] }),
    sp("final", "orch", "llm", 121, 199),
  ];

  it("folds steps into their agents and keeps agent-to-agent messages", () => {
    const tree = buildTree(swarm);
    const g = agentGraph(tree, flowEdges(tree));
    expect(g.nodes.map((n) => n.span_id).sort()).toEqual(["checker", "critic", "fork", "join", "msg", "orch", "r1", "r2", "split"]);
    const edges = g.edges.map((e) => `${e.source}>${e.target}${e.link ? "~" : ""}`).sort();
    expect(edges).toEqual(
      ["msg>orch", "orch>split", "split>r1", "split>r2", "r1>join", "r2>join", "join>fork", "fork>checker", "fork>critic", "checker>critic~", "checker>orch", "critic>orch"].sort(),
    );
    expect(g.owned.get("r1")!.map((s) => s.span_id)).toEqual(["r1", "r1.s"]);
  });

  it("draws one agent as message -> agent", () => {
    const spans = [sp("msg", undefined, "message", 0, 0), sp("a", "msg", "agent", 1, 9, { agent: "a" }), sp("l", "a", "llm", 2, 5), sp("t", "a", "tool", 6, 8)];
    const tree = buildTree(spans);
    const g = agentGraph(tree, flowEdges(tree));
    expect(g.edges.map((e) => e.id)).toEqual(["msg->a"]);
  });

  it("marks edges back to an owning agent as returns and keeps them out of lineage", () => {
    const tree = buildTree(swarm);
    const g = agentGraph(tree, flowEdges(tree));
    expect(g.edges.filter((e) => e.back).map((e) => e.id).sort()).toEqual(["checker->orch", "critic->orch"]);
    const ids = lineage("r1", g.edges);
    expect(ids.has("msg") && ids.has("split") && ids.has("join") && ids.has("critic")).toBe(true);
    expect(ids.has("r2")).toBe(false);
    expect(lineage("checker", g.edges).has("msg")).toBe(true);
  });

  it("finds upstream and downstream lineage", () => {
    const tree = buildTree(swarm);
    const ids = lineage("r1", flowEdges(tree));
    expect(ids.has("msg") && ids.has("split") && ids.has("r1.s") && ids.has("join") && ids.has("final")).toBe(true);
    expect(ids.has("r2")).toBe(false);
  });
});

describe("filter", () => {
  const spans = [
    sp("a", undefined, "tool", 0, 1, { name: "web_search", input: { query: "solid state" } }),
    sp("b", undefined, "llm", 0, 1, { name: "summarize", status: "error", model: "claude-haiku-5-5" }),
  ];
  const run = (q: string, agent = "researcher") => spans.filter((s) => compileFilter(q)(s, agent)).map((s) => s.span_id);

  it("matches plain words against names and payloads", () => {
    expect(run("")).toEqual(["a", "b"]);
    expect(run("web")).toEqual(["a"]);
    expect(run("search")).toEqual(["a", "b"]); // agent "researcher" contains it
    expect(run('"solid state"')).toEqual(["a"]);
  });

  it("matches fields and negation", () => {
    expect(run("status:error")).toEqual(["b"]);
    expect(run("-status:error")).toEqual(["a"]);
    expect(run("kind:llm model:haiku")).toEqual(["b"]);
    expect(run("agent:research")).toEqual(["a", "b"]);
    expect(run("agent:billing")).toEqual([]);
  });

  it("treats prototype names as unknown keys", () => {
    for (const q of ["constructor:x", "toString:x", "__proto__:x", "hasOwnProperty:x"]) expect(run(q)).toEqual([]);
  });

  it("skips terms that have no value yet", () => {
    expect(run("-")).toEqual(["a", "b"]);
    expect(run("agent:")).toEqual(["a", "b"]);
    expect(run("-kind:")).toEqual(["a", "b"]);
    expect(run('web "')).toEqual(["a"]);
  });

  it("sees an agent that becomes known after the first search", () => {
    // OTel exports children first, so a span's agent can arrive with its parent later.
    const child = sp("c", "p", "llm", 1, 2, { name: "summarize" });
    const f = compileFilter("researcher");
    expect(f(child, undefined)).toBe(false);
    expect(f(child, "researcher")).toBe(true);
  });
});

describe("helpers", () => {
  it("packs overlapping spans into separate lanes", () => {
    const spans = [sp("a", undefined, "tool", 0, 10), sp("b", undefined, "tool", 5, 15), sp("c", undefined, "tool", 10, 20)];
    expect(packLanes(spans, 100)).toEqual([0, 1, 0]);
  });

  it("computes self time without double counting overlapping children", () => {
    const parent = sp("p", undefined, "agent", 0, 100);
    const kids = [sp("a", "p", "tool", 10, 40), sp("b", "p", "tool", 30, 50), sp("c", "p", "tool", 90, 120)];
    expect(selfTime(parent, kids, 0)).toBe(100 - 40 - 10);
  });

  it("ranks nodes by longest path and cuts cycles", () => {
    const spans = [sp("a", undefined, "step", 0, 1), sp("b", undefined, "step", 1, 2), sp("c", undefined, "step", 2, 3), sp("d", undefined, "step", 3, 4)];
    const e = (source: string, target: string, back = false) => ({ id: `${source}->${target}`, source, target, link: false, back, count: 1 });
    const ranks = (edges: ReturnType<typeof e>[]) => Object.fromEntries(flowRanks(spans, edges));
    expect(ranks([e("a", "b"), e("b", "c"), e("a", "c"), e("c", "d")])).toEqual({ a: 0, b: 1, c: 2, d: 3 });
    expect(ranks([e("a", "b"), e("b", "c"), e("c", "a", true)])).toEqual({ a: 0, b: 1, c: 2, d: 0 });
    // Without the back flag, the cycle b -> c -> b is cut at b.
    expect(ranks([e("a", "b"), e("b", "c"), e("c", "b"), e("d", "c")])).toEqual({ a: 0, b: 1, c: 2, d: 0 });
    expect(flowRanks(spans, [e("b", "c"), e("c", "b")]).size).toBe(4);
  });

  it("formats how long ago something happened", () => {
    expect([fmtAgo(30_000), fmtAgo(5 * 60_000), fmtAgo(338 * 60_000), fmtAgo(50 * 3_600_000)]).toEqual(["just now", "5 min ago", "5 h ago", "2 d ago"]);
  });

  it("picks 1-2-5 tick steps", () => {
    expect(niceStep(1000)).toBe(200);
    expect(niceStep(12_000)).toBe(2000);
    expect(niceStep(450)).toBe(100);
  });
});
