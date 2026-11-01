import { useState, type ReactNode } from "react";
import { agentColor, agentOf, ancestors, endOf, fmtDuration, selfTime, type Span, type Tree } from "../model";
import { JsonTree } from "./JsonTree";
import { KindIcon } from "./KindIcon";

const TABS = ["Headers", "Payload", "Preview", "Response", "Initiator", "Timing"] as const;
type Tab = (typeof TABS)[number];

interface Props {
  span: Span;
  tree: Tree;
  now: number;
  onSelect: (id: string) => void;
  onClose: () => void;
}

export function SpanDetails({ span, tree, now, onSelect, onClose }: Props) {
  const [tab, setTab] = useState<Tab>("Headers");
  return (
    <section className="details">
      <div className="tabs">
        <button className="close" title="Close" onClick={onClose}>
          ✕
        </button>
        {TABS.map((t) => (
          <button key={t} className={t === tab ? "tab active" : "tab"} onClick={() => setTab(t)}>
            {t}
          </button>
        ))}
      </div>
      <div className="details-body">
        {tab === "Headers" && <Headers span={span} tree={tree} now={now} onSelect={onSelect} />}
        {tab === "Payload" && <Payload value={span.input} empty="This span has no input." title={span.kind === "llm" ? "Prompt" : "Input"} />}
        {tab === "Preview" && <Preview value={span.output} error={span.error} />}
        {tab === "Response" && <Raw value={span.output} empty="This span has no output." />}
        {tab === "Initiator" && <Initiator span={span} tree={tree} onSelect={onSelect} />}
        {tab === "Timing" && <Timing span={span} tree={tree} now={now} onSelect={onSelect} />}
      </div>
    </section>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  const [open, setOpen] = useState(true);
  return (
    <div className="section">
      <div className="section-title" onClick={() => setOpen(!open)}>
        {open ? "▾" : "▸"} {title}
      </div>
      {open && <div className="section-body">{children}</div>}
    </div>
  );
}

function Row({ k, children }: { k: string; children: ReactNode }) {
  return (
    <div className="kv">
      <div className="k">{k}:</div>
      <div className="v">{children}</div>
    </div>
  );
}

function SpanLink({ span, onSelect }: { span: Span; onSelect: (id: string) => void }) {
  return (
    <button className="span-link" onClick={() => onSelect(span.span_id)}>
      <KindIcon kind={span.kind} /> {span.name}
    </button>
  );
}

function Headers({ span, tree, now, onSelect }: { span: Span; tree: Tree; now: number; onSelect: (id: string) => void }) {
  const agent = agentOf(span, tree);
  const parent = span.parent_id ? tree.byId.get(span.parent_id) : undefined;
  const t0 = tree.spans[0]?.start ?? span.start;
  const attrs = Object.entries(span.attrs ?? {}).sort(([a], [b]) => a.localeCompare(b));
  return (
    <>
      <Section title="General">
        <Row k="Name">{span.name}</Row>
        <Row k="Kind">
          <KindIcon kind={span.kind} /> {span.kind}
        </Row>
        <Row k="Status">
          <span className={`status-${span.status}`}>{span.status}</span>
          {span.error && <span className="status-error"> — {span.error}</span>}
        </Row>
        {agent && (
          <Row k="Agent">
            <span style={{ color: agentColor(agent) }}>● </span>
            {agent}
            {!span.agent && <span className="muted"> (inherited)</span>}
          </Row>
        )}
        {span.model && <Row k="Model">{span.model}</Row>}
        {(span.tokens_in !== undefined || span.tokens_out !== undefined) && (
          <Row k="Tokens">
            {span.tokens_in ?? 0} in · {span.tokens_out ?? 0} out
          </Row>
        )}
        <Row k="Started">
          {new Date(span.start).toLocaleString()} (+{fmtDuration(span.start - t0)})
        </Row>
        <Row k="Duration">{span.end === undefined ? `${fmtDuration(endOf(span, now) - span.start)} (running)` : fmtDuration(span.end - span.start)}</Row>
        {parent && (
          <Row k="Parent">
            <SpanLink span={parent} onSelect={onSelect} />
          </Row>
        )}
        <Row k="Span ID">
          <code>{span.span_id}</code>
        </Row>
        <Row k="Trace ID">
          <code>{span.trace_id}</code>
        </Row>
      </Section>
      {attrs.length > 0 && (
        <Section title={`Attributes (${attrs.length})`}>
          {attrs.map(([k, v]) => (
            <Row key={k} k={k}>
              <code>{typeof v === "string" ? v : JSON.stringify(v)}</code>
            </Row>
          ))}
        </Section>
      )}
    </>
  );
}

function Payload({ value, empty, title }: { value: unknown; empty: string; title: string }) {
  const [source, setSource] = useState(false);
  if (value === undefined || value === null) return <p className="empty">{empty}</p>;
  const structured = typeof value === "object";
  return (
    <Section title={title}>
      {structured && (
        <button className="link-btn" onClick={() => setSource(!source)}>
          {source ? "view parsed" : "view source"}
        </button>
      )}
      {structured && !source ? <JsonTree value={value} /> : <Raw value={value} empty={empty} />}
    </Section>
  );
}

function Preview({ value, error }: { value: unknown; error?: string }) {
  return (
    <>
      {error && <div className="error-banner">{error}</div>}
      {value === undefined || value === null ? (
        !error && <p className="empty">This span has no output.</p>
      ) : typeof value === "string" ? (
        <div className="text-preview">{value}</div>
      ) : (
        <JsonTree value={value} />
      )}
    </>
  );
}

function Raw({ value, empty }: { value: unknown; empty: string }) {
  if (value === undefined || value === null) return <p className="empty">{empty}</p>;
  return <pre className="raw">{typeof value === "string" ? value : JSON.stringify(value, null, 2)}</pre>;
}

function Initiator({ span, tree, onSelect }: { span: Span; tree: Tree; onSelect: (id: string) => void }) {
  const chain = ancestors(span, tree);
  const linkedFrom = (span.links ?? []).map((id) => tree.byId.get(id)).filter((s): s is Span => !!s);
  const linkedTo = tree.spans.filter((s) => s.links?.includes(span.span_id));
  const initiated = (s: Span, depth: number): ReactNode[] =>
    (tree.children.get(s.span_id) ?? []).flatMap((c) => [
      <div key={c.span_id} style={{ paddingLeft: depth * 16 }}>
        <SpanLink span={c} onSelect={onSelect} />
      </div>,
      ...initiated(c, depth + 1),
    ]);
  const kids = initiated(span, 0);
  return (
    <>
      <Section title="Initiator chain">
        {chain.length === 0 && <p className="empty">This span is a root.</p>}
        {chain.map((s, i) => (
          <div key={s.span_id} style={{ paddingLeft: i * 16 }}>
            <SpanLink span={s} onSelect={onSelect} />
          </div>
        ))}
        {chain.length > 0 && (
          <div style={{ paddingLeft: chain.length * 16 }} className="current">
            <KindIcon kind={span.kind} /> {span.name}
          </div>
        )}
      </Section>
      <Section title={`Initiated by this span (${kids.length})`}>{kids.length ? kids : <p className="empty">Nothing.</p>}</Section>
      {(linkedFrom.length > 0 || linkedTo.length > 0) && (
        <Section title="Messages">
          {linkedFrom.map((s) => (
            <div key={s.span_id}>
              received from <SpanLink span={s} onSelect={onSelect} />
            </div>
          ))}
          {linkedTo.map((s) => (
            <div key={s.span_id}>
              sent to <SpanLink span={s} onSelect={onSelect} />
            </div>
          ))}
        </Section>
      )}
    </>
  );
}

function Timing({ span, tree, now, onSelect }: { span: Span; tree: Tree; now: number; onSelect: (id: string) => void }) {
  const parent = span.parent_id ? tree.byId.get(span.parent_id) : undefined;
  const t0 = tree.spans[0]?.start ?? span.start;
  const kids = tree.children.get(span.span_id) ?? [];
  const end = endOf(span, now);
  const total = Math.max(end - span.start, 1e-6);
  const bar = (a: number, b: number, cls: string) => (
    <div className="timing-track">
      <div className={`timing-bar ${cls}`} style={{ left: `${((a - span.start) / total) * 100}%`, width: `${Math.max(((b - a) / total) * 100, 0.5)}%` }} />
    </div>
  );
  return (
    <div className="timing">
      <div className="timing-head">Started {fmtDuration(span.start - t0)} into the trace</div>
      <div className="timing-group">Resource scheduling</div>
      <div className="timing-row">
        <span>Queueing</span>
        <span />
        <span>{parent ? fmtDuration(span.start - parent.start) : "—"}</span>
      </div>
      <div className="timing-group">Execution</div>
      {kids.map((k) => (
        <div className="timing-row" key={k.span_id}>
          <SpanLink span={k} onSelect={onSelect} />
          {bar(Math.max(k.start, span.start), Math.min(endOf(k, now), end), `kind-${k.kind} ${k.status}`)}
          <span>{fmtDuration(endOf(k, now) - k.start)}</span>
        </div>
      ))}
      <div className="timing-row">
        <span>Self time</span>
        <span />
        <span>{fmtDuration(selfTime(span, kids, now))}</span>
      </div>
      <div className="timing-row total">
        <span>Total</span>
        {bar(span.start, end, `kind-${span.kind}`)}
        <span>{fmtDuration(end - span.start)}</span>
      </div>
    </div>
  );
}
