// Pure data logic shared by both views: span tree, flow graph, filters, formatting.

export type Kind = "message" | "agent" | "llm" | "tool" | "split" | "fork" | "join" | "reasoning" | "step";
export type Status = "running" | "ok" | "error";

export interface Span {
  trace_id: string;
  span_id: string;
  parent_id?: string;
  kind: Kind;
  name: string;
  agent?: string;
  start: number;
  end?: number;
  status: Status;
  error?: string;
  input?: unknown;
  output?: unknown;
  model?: string;
  tokens_in?: number;
  tokens_out?: number;
  attrs?: Record<string, unknown>;
  links?: string[];
  seq: number;
}

export interface TraceSummary {
  trace_id: string;
  name: string;
  start: number;
  end: number | null;
  status: Status;
  spans: number;
  errors: number;
  agents: string[];
  tokens: number;
  source: string | null;
  seq: number;
}

export const KIND_COLOR: Record<Kind, string> = {
  message: "#8ab4f8",
  agent: "#c58af9",
  llm: "#81c995",
  tool: "#fdd663",
  split: "#fcad70",
  fork: "#ff8bcb",
  join: "#78d9ec",
  reasoning: "#a8a2ff",
  step: "#9aa0a6",
};

export const KIND_GROUPS: { id: string; label: string; kinds: Kind[] }[] = [
  { id: "message", label: "Msg", kinds: ["message"] },
  { id: "agent", label: "Agent", kinds: ["agent"] },
  { id: "llm", label: "LLM", kinds: ["llm"] },
  { id: "reasoning", label: "Reasoning", kinds: ["reasoning"] },
  { id: "tool", label: "Tool", kinds: ["tool"] },
  { id: "flow", label: "Split/Fork", kinds: ["split", "fork", "join"] },
  { id: "other", label: "Other", kinds: ["step"] },
];

const AGENT_PALETTE = ["#c58af9", "#8ab4f8", "#81c995", "#fdd663", "#f28b82", "#78d9ec", "#fcad70", "#ff8bcb"];

export function agentColor(name: string | undefined): string {
  if (!name) return "#9aa0a6";
  let h = 0;
  for (const c of name) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return AGENT_PALETTE[h % AGENT_PALETTE.length];
}

// ---- tree ----

export interface Tree {
  spans: Span[];
  byId: Map<string, Span>;
  children: Map<string, Span[]>;
  roots: Span[];
}

const byStart = (a: Span, b: Span) => a.start - b.start || a.seq - b.seq;

export function buildTree(spans: Iterable<Span>): Tree {
  const list = [...spans].sort(byStart);
  const byId = new Map(list.map((s) => [s.span_id, s]));
  const children = new Map<string, Span[]>();
  const roots: Span[] = [];
  for (const s of list) {
    if (s.parent_id && s.parent_id !== s.span_id && byId.has(s.parent_id)) {
      const kids = children.get(s.parent_id);
      if (kids) kids.push(s);
      else children.set(s.parent_id, [s]);
    } else {
      roots.push(s);
    }
  }
  return { spans: list, byId, children, roots };
}

/** Root first, parent last. Stops on parent cycles in bad data. */
export function ancestors(span: Span, tree: Tree): Span[] {
  const out: Span[] = [];
  const seen = new Set([span.span_id]);
  let cur = span.parent_id ? tree.byId.get(span.parent_id) : undefined;
  while (cur && !seen.has(cur.span_id)) {
    seen.add(cur.span_id);
    out.unshift(cur);
    cur = cur.parent_id ? tree.byId.get(cur.parent_id) : undefined;
  }
  return out;
}

/** The span's own agent, else the nearest ancestor's. */
export function agentOf(span: Span, tree: Tree): string | undefined {
  if (span.agent) return span.agent;
  const anc = ancestors(span, tree);
  for (let i = anc.length - 1; i >= 0; i--) if (anc[i].agent) return anc[i].agent;
  return undefined;
}

export function descendants(span: Span, tree: Tree): Span[] {
  const out: Span[] = [];
  const seen = new Set<string>();
  const stack = [...(tree.children.get(span.span_id) ?? [])];
  while (stack.length) {
    const s = stack.pop()!;
    if (seen.has(s.span_id)) continue;
    seen.add(s.span_id);
    out.push(s);
    for (const k of tree.children.get(s.span_id) ?? []) stack.push(k);
  }
  return out;
}

export const endOf = (s: Span, now: number) => s.end ?? Math.max(now, s.start);

// ---- flow graph ----

export interface FlowEdge {
  id: string;
  source: string;
  target: string;
  /** Explicit message passed between spans (span.links), not just sequence. */
  link: boolean;
  /** Work returning to an agent that contains the source (agent view only). */
  back: boolean;
  count: number;
}

const PARALLEL: ReadonlySet<Kind> = new Set(["split", "fork"]);

/**
 * For siblings in start order, the siblings each one follows: the latest to
 * have finished by its start. One that finished before another of them began
 * is superseded. Running spans never finish, so they gate nothing.
 */
function siblingPreds(kids: Span[]): Span[][] {
  const done: Span[] = []; // started siblings that have an end, by end; swept up to `head`
  let head = 0;
  let frontier: Span[] = [];
  return kids.map((k) => {
    while (head < done.length && done[head].end! <= k.start) {
      const s = done[head++];
      frontier = [...frontier.filter((f) => f.end! > s.start), s];
    }
    if (k.end !== undefined) {
      let i = done.length;
      while (i > head && done[i - 1].end! > k.end) i--;
      done.splice(i, 0, k);
    }
    return frontier;
  });
}

/**
 * How work flowed: a child follows the siblings that had finished by its
 * start (their last steps, if those had ended by then), else its parent; a
 * span's own last steps are those of the children nothing followed. Children
 * of a split or fork are always parallel branches. A single agent therefore
 * reads as a straight chain.
 */
export function flowEdges(tree: Tree): FlowEdge[] {
  const edges = new Map<string, FlowEdge>();
  const add = (source: string, target: string, link: boolean) => {
    const id = `${source}->${target}`;
    const e = edges.get(id);
    if (e) e.link ||= link;
    else edges.set(id, { id, source, target, link, back: false, count: 1 });
  };
  const seen = new Set<string>();
  const walk = (span: Span): string[] => {
    if (seen.has(span.span_id)) return [span.span_id];
    seen.add(span.span_id);
    const kids = tree.children.get(span.span_id) ?? [];
    if (!kids.length) return [span.span_id];
    const preds: Span[][] = PARALLEL.has(span.kind) ? kids.map(() => []) : siblingPreds(kids);
    const exits = new Map<Span, string[]>();
    const followed = new Set<Span>();
    kids.forEach((k, i) => {
      if (!preds[i].length) add(span.span_id, k.span_id, false);
      for (const p of preds[i]) {
        followed.add(p);
        // A step still running when k starts (e.g. a background subagent's) did not lead into k.
        for (const x of exits.get(p)!) add((tree.byId.get(x)!.end ?? Infinity) <= k.start ? x : p.span_id, k.span_id, false);
      }
      exits.set(k, walk(k));
    });
    return kids.filter((k) => !followed.has(k)).flatMap((k) => exits.get(k)!);
  };
  tree.roots.forEach(walk);
  for (const s of tree.spans) {
    for (const l of s.links ?? []) if (l !== s.span_id && tree.byId.has(l)) add(l, s.span_id, true);
  }
  return [...edges.values()];
}

const AGENT_VIEW_KINDS: ReadonlySet<Kind> = new Set(["message", "agent", "split", "fork", "join"]);

export interface AgentGraph {
  nodes: Span[];
  edges: FlowEdge[];
  /** Every span folded into each node, including the node itself. */
  owned: Map<string, Span[]>;
}

/** Folds llm/tool/step spans into their nearest agent (or message/split/fork/join) and projects edges onto those owners. */
export function agentGraph(tree: Tree, edges: FlowEdge[]): AgentGraph {
  const ownerCache = new Map<string, Span>();
  const owner = (s: Span): Span => {
    const hit = ownerCache.get(s.span_id);
    if (hit) return hit;
    let cur = s;
    if (!AGENT_VIEW_KINDS.has(s.kind)) {
      const anc = ancestors(s, tree);
      for (let i = anc.length - 1; i >= 0; i--) {
        cur = anc[i];
        if (AGENT_VIEW_KINDS.has(cur.kind)) break;
      }
    }
    ownerCache.set(s.span_id, cur);
    return cur;
  };
  const owned = new Map<string, Span[]>();
  for (const s of tree.spans) {
    const o = owner(s).span_id;
    const list = owned.get(o);
    if (list) list.push(s);
    else owned.set(o, [s]);
  }
  const out = new Map<string, FlowEdge>();
  for (const e of edges) {
    const a = owner(tree.byId.get(e.source)!);
    const b = owner(tree.byId.get(e.target)!);
    if (a === b) continue;
    const id = `${a.span_id}->${b.span_id}`;
    const prev = out.get(id);
    if (prev) {
      prev.count += 1;
      prev.link ||= e.link;
    } else {
      const back = ancestors(a, tree).includes(b);
      out.set(id, { id, source: a.span_id, target: b.span_id, link: e.link, back, count: 1 });
    }
  }
  const nodes = tree.spans.filter((s) => owned.has(s.span_id));
  return { nodes, edges: [...out.values()], owned };
}

/** Span ids upstream and downstream of `id` along the edges, including `id`. Return edges are not followed. */
export function lineage(id: string, edges: FlowEdge[]): Set<string> {
  const fwd = new Map<string, string[]>();
  const back = new Map<string, string[]>();
  for (const e of edges) {
    if (e.back) continue;
    (fwd.get(e.source) ?? fwd.set(e.source, []).get(e.source)!).push(e.target);
    (back.get(e.target) ?? back.set(e.target, []).get(e.target)!).push(e.source);
  }
  const out = new Set([id]);
  // Each direction needs its own visited set: with a cycle, one walk would
  // otherwise stop the other at nodes it already reached.
  for (const adj of [fwd, back]) {
    const seen = new Set([id]);
    const stack = [id];
    while (stack.length) {
      for (const n of adj.get(stack.pop()!) ?? []) {
        if (!seen.has(n)) {
          seen.add(n);
          out.add(n);
          stack.push(n);
        }
      }
    }
  }
  return out;
}

// ---- filtering ----

const textCache = new WeakMap<Span, string>();

/** The agent is left out: it is inherited, and a parent that arrives later changes it. */
function searchText(s: Span): string {
  let t = textCache.get(s);
  if (t === undefined) {
    const payload = (v: unknown) => (v === undefined ? "" : typeof v === "string" ? v : JSON.stringify(v));
    t = [s.name, s.kind, s.model, s.error, payload(s.input), payload(s.output)].join("\n").toLowerCase();
    textCache.set(s, t);
  }
  return t;
}

/**
 * DevTools-style filter: plain words match name/agent/payloads, `key:value`
 * matches a field (agent, kind, status, model, name), `-` negates. All terms must hold.
 * Terms with no value yet (`-`, `agent:` while typing) are ignored.
 */
export function compileFilter(query: string): (s: Span, agent: string | undefined) => boolean {
  const terms = [...query.matchAll(/(-?)(?:(\w+):)?("[^"]*"|\S*)/g)]
    .map(([, neg, key, raw]) => ({
      neg: neg === "-",
      key: key?.toLowerCase(),
      value: raw.replace(/^"|"$/g, "").toLowerCase(),
    }))
    .filter((t) => t.value);
  if (!terms.length) return () => true;
  return (s, agent) =>
    terms.every(({ neg, key, value }) => {
      const field: Record<string, string | undefined> = { agent, kind: s.kind, type: s.kind, status: s.status, model: s.model, name: s.name };
      if (key && Object.hasOwn(field, key)) return (field[key] ?? "").toLowerCase().includes(value) !== neg;
      const text = key ? `${key}:${value}` : value;
      return (searchText(s).includes(text) || !!agent?.toLowerCase().includes(text)) !== neg;
    });
}

export const overlaps = (s: Span, from: number, to: number, now: number) => s.start <= to && endOf(s, now) >= from;

// ---- layout helpers ----

/** Greedy interval packing: lane index per span (same order as input). */
export function packLanes(spans: Span[], now: number): number[] {
  const laneEnds: number[] = [];
  return spans.map((s) => {
    let lane = laneEnds.findIndex((e) => e <= s.start);
    if (lane < 0) lane = laneEnds.push(0) - 1;
    laneEnds[lane] = endOf(s, now);
    return lane;
  });
}

/**
 * Longest-path rank of each node from the roots along `edges` (return edges
 * ignored), in linear time. A cycle is cut at its earliest node in `nodes` order.
 */
export function flowRanks(nodes: Span[], edges: FlowEdge[]): Map<string, number> {
  const rank = new Map(nodes.map((n) => [n.span_id, 0]));
  const succ = new Map<string, string[]>();
  const indeg = new Map<string, number>();
  for (const e of edges) {
    if (e.back || e.source === e.target || !rank.has(e.source) || !rank.has(e.target)) continue;
    (succ.get(e.source) ?? succ.set(e.source, []).get(e.source)!).push(e.target);
    indeg.set(e.target, (indeg.get(e.target) ?? 0) + 1);
  }
  const queued = new Set<string>();
  const queue: string[] = [];
  const enqueue = (id: string) => {
    queued.add(id);
    queue.push(id);
  };
  for (const n of nodes) if (!indeg.get(n.span_id)) enqueue(n.span_id);
  let next = 0;
  for (let head = 0; head < nodes.length; head++) {
    if (head === queue.length) {
      // Only cycles are left.
      while (queued.has(nodes[next].span_id)) next++;
      enqueue(nodes[next].span_id);
    }
    const r = rank.get(queue[head])! + 1;
    for (const t of succ.get(queue[head]) ?? []) {
      if (queued.has(t)) continue;
      if (r > rank.get(t)!) rank.set(t, r);
      const d = indeg.get(t)! - 1;
      indeg.set(t, d);
      if (!d) enqueue(t);
    }
  }
  return rank;
}

/** Duration not covered by any child span. */
export function selfTime(span: Span, kids: Span[], now: number): number {
  const end = endOf(span, now);
  const iv = kids.map((k) => [Math.max(k.start, span.start), Math.min(endOf(k, now), end)]).filter(([a, b]) => b > a).sort((x, y) => x[0] - y[0]);
  let covered = 0;
  let curA = -Infinity;
  let curB = -Infinity;
  for (const [a, b] of iv) {
    if (a > curB) {
      if (curB > curA) covered += curB - curA;
      [curA, curB] = [a, b];
    } else curB = Math.max(curB, b);
  }
  if (curB > curA) covered += curB - curA;
  return end - span.start - covered;
}

export function niceStep(range: number, target = 6): number {
  if (!(range > 0)) return 1;
  const raw = range / target;
  const pow = 10 ** Math.floor(Math.log10(raw));
  const n = raw / pow;
  return (n < 1.5 ? 1 : n < 3.5 ? 2 : n < 7.5 ? 5 : 10) * pow;
}

// ---- formatting ----

export function fmtDuration(ms: number): string {
  if (!isFinite(ms)) return "";
  if (ms < 10) return `${ms.toFixed(1)} ms`;
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(2)} s`;
  return `${(ms / 60_000).toFixed(1)} min`;
}

export function fmtAgo(ms: number): string {
  const min = Math.floor(ms / 60_000);
  if (min < 1) return "just now";
  if (min < 60) return `${min} min ago`;
  const h = Math.floor(min / 60);
  return h < 24 ? `${h} h ago` : `${Math.floor(h / 24)} d ago`;
}

export function fmtCount(n: number): string {
  return n < 1000 ? String(n) : n < 1e6 ? `${(n / 1000).toFixed(1)}k` : `${(n / 1e6).toFixed(1)}M`;
}

export function fmtBytes(n: number): string {
  return n < 1024 ? `${n} B` : n < 1024 ** 2 ? `${(n / 1024).toFixed(1)} kB` : `${(n / 1024 ** 2).toFixed(1)} MB`;
}

export function fmtClock(ms: number): string {
  const d = new Date(ms);
  const time = d.toLocaleTimeString([], { hour12: false });
  return d.toDateString() === new Date().toDateString() ? time : `${d.toLocaleDateString()} ${time}`;
}

const sizeCache = new WeakMap<Span, number>();

/** Approximate payload bytes (input + output as JSON). */
export function payloadBytes(s: Span): number {
  let n = sizeCache.get(s);
  if (n === undefined) {
    const len = (v: unknown) => (v === undefined ? 0 : new TextEncoder().encode(typeof v === "string" ? v : JSON.stringify(v)).length);
    n = len(s.input) + len(s.output);
    sizeCache.set(s, n);
  }
  return n;
}

export function preview(v: unknown, max = 80): string {
  if (v === undefined || v === null) return "";
  const s = typeof v === "string" ? v : JSON.stringify(v);
  return s.length > max ? `${s.slice(0, max)}…` : s;
}
