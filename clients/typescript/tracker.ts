// Agent Tracker client for Node. No dependencies.
//
//   const t = new Tracker();
//   await t.trace("Plan a trip", prompt, () =>
//     t.agent("planner", { input: prompt }, async () => {
//       const plan = await t.llm("plan", { model: "claude-sonnet-5-5" }, callModel);
//       return t.tool("search", { input: plan.query }, () => search(plan.query));
//     }));
//   await t.flush();
//
// Spans nest by async context, so parallel branches (Promise.all) inside a
// fork or split each become their own child span.

import { AsyncLocalStorage } from "node:async_hooks";
import { randomBytes, randomUUID } from "node:crypto";

export type Kind = "message" | "agent" | "llm" | "tool" | "split" | "fork" | "join" | "step";

export interface SpanOptions {
  agent?: string;
  input?: unknown;
  model?: string;
  attrs?: Record<string, unknown>;
  /** Spans whose output this span consumes (messages between agents). */
  links?: Array<Span | string>;
}

type Body<T> = (span: Span) => T | Promise<T>;

const now = () => performance.timeOrigin + performance.now();
const linkIds = (links: Array<Span | string>) => links.map((l) => (typeof l === "string" ? l : l.id));

export class Span {
  readonly traceId: string;
  readonly id: string;
  private fields: Record<string, unknown> = {};
  private ended = false;
  private readonly tracker: Tracker;

  constructor(tracker: Tracker, traceId: string, id: string) {
    this.tracker = tracker;
    this.traceId = traceId;
    this.id = id;
  }

  output(value: unknown): this {
    this.fields.output = value;
    return this;
  }

  /** Sets the output only if the body did not set one itself. */
  outputIfUnset(value: unknown): this {
    if (!("output" in this.fields)) this.fields.output = value;
    return this;
  }

  tokens(input: number, output: number): this {
    this.fields.tokens_in = input;
    this.fields.tokens_out = output;
    return this;
  }

  model(name: string): this {
    this.fields.model = name;
    return this;
  }

  attr(key: string, value: unknown): this {
    this.fields.attrs = { ...(this.fields.attrs as object), [key]: value };
    return this;
  }

  link(...spans: Array<Span | string>): this {
    this.fields.links = [...((this.fields.links as string[]) ?? []), ...linkIds(spans)];
    return this;
  }

  end(error?: unknown): void {
    if (this.ended) return;
    this.ended = true;
    const failed = error !== undefined;
    this.tracker.send({
      ...this.fields,
      trace_id: this.traceId,
      span_id: this.id,
      end: now(),
      status: failed ? "error" : "ok",
      ...(failed ? { error: error instanceof Error ? error.message : String(error) } : {}),
    });
  }
}

export class Tracker {
  readonly url: string;
  readonly source: string;
  private readonly context = new AsyncLocalStorage<Span | undefined>();
  private readonly pending = new Set<Promise<void>>();
  private warned = false;

  constructor(opts: { url?: string; source?: string } = {}) {
    this.url = (opts.url ?? process.env.AGENT_TRACKER_URL ?? "http://127.0.0.1:4318").replace(/\/$/, "");
    this.source = opts.source ?? "sdk:typescript";
  }

  current(): Span | undefined {
    return this.context.getStore();
  }

  /** Starts a span under the current one. Prefer `run` and the kind helpers, which end it for you. */
  start(kind: Kind, name: string, opts: SpanOptions = {}): Span {
    const parent = this.current();
    const span = new Span(this, parent?.traceId ?? randomUUID(), randomBytes(8).toString("hex"));
    const attrs = parent ? opts.attrs : { source: this.source, ...opts.attrs };
    this.send({
      trace_id: span.traceId,
      span_id: span.id,
      parent_id: parent?.id,
      kind,
      name,
      agent: opts.agent ?? (kind === "agent" ? name : undefined),
      start: now(),
      input: opts.input,
      model: opts.model,
      attrs,
      links: opts.links && linkIds(opts.links),
    });
    return span;
  }

  /** Runs `fn` inside a new span. Its return value becomes the span output unless one was set. */
  async run<T>(kind: Kind, name: string, opts: SpanOptions, fn: Body<T>): Promise<T> {
    const span = this.start(kind, name, opts);
    try {
      const result = await this.context.run(span, () => fn(span));
      if (result !== undefined) span.outputIfUnset(result);
      span.end();
      return result;
    } catch (err) {
      span.end(err ?? "error");
      throw err;
    }
  }

  /** A new trace rooted at the message you sent. */
  trace<T>(name: string, message: unknown, fn: Body<T>): Promise<T> {
    return this.context.run(undefined, () =>
      this.run("message", name, { input: message }, async (span) => {
        span.end();
        return fn(span);
      }),
    );
  }

  agent<T>(name: string, opts: SpanOptions, fn: Body<T>) { return this.run("agent", name, opts, fn); }
  llm<T>(name: string, opts: SpanOptions, fn: Body<T>) { return this.run("llm", name, opts, fn); }
  tool<T>(name: string, opts: SpanOptions, fn: Body<T>) { return this.run("tool", name, opts, fn); }
  /** Children run as parallel branches, each handling one part of the input. */
  split<T>(name: string, opts: SpanOptions, fn: Body<T>) { return this.run("split", name, opts, fn); }
  /** Children run as parallel branches over the same input. */
  fork<T>(name: string, opts: SpanOptions, fn: Body<T>) { return this.run("fork", name, opts, fn); }
  join<T>(name: string, opts: SpanOptions, fn: Body<T>) { return this.run("join", name, opts, fn); }
  step<T>(name: string, opts: SpanOptions, fn: Body<T>) { return this.run("step", name, opts, fn); }

  send(update: Record<string, unknown>): void {
    const req = fetch(`${this.url}/api/spans`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(update),
    })
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
      })
      .catch((err) => {
        if (!this.warned) console.warn(`[agent-tracker] cannot reach ${this.url}: ${err.message}`);
        this.warned = true;
      })
      .finally(() => this.pending.delete(req));
    this.pending.add(req);
  }

  /** Waits for every update sent so far. */
  async flush(): Promise<void> {
    while (this.pending.size) await Promise.all([...this.pending]);
  }
}
