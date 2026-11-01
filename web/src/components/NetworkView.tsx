import { useMemo, useState, type KeyboardEvent, type MouseEvent } from "react";
import {
  agentColor,
  agentOf,
  ancestors,
  compileFilter,
  endOf,
  fmtBytes,
  fmtCount,
  fmtDuration,
  KIND_GROUPS,
  niceStep,
  overlaps,
  payloadBytes,
  type Span,
  type TraceSummary,
  type Tree,
} from "../model";
import { KindIcon } from "./KindIcon";
import { Overview } from "./Overview";
import { SpanDetails } from "./SpanDetails";

type Col = "name" | "status" | "type" | "initiator" | "size" | "time" | "waterfall";

const COLUMNS: { id: Col; label: string }[] = [
  { id: "name", label: "Name" },
  { id: "status", label: "Status" },
  { id: "type", label: "Type" },
  { id: "initiator", label: "Initiator" },
  { id: "size", label: "Size" },
  { id: "time", label: "Time" },
  { id: "waterfall", label: "Waterfall" },
];
const GRID = "minmax(200px, 2.4fr) 72px 64px minmax(120px, 1.3fr) 72px 72px minmax(220px, 3fr)";

export interface ViewProps {
  trace: TraceSummary;
  tree: Tree;
  now: number;
  selected: string | null;
  onSelect: (id: string | null) => void;
  clip: [number, number] | null;
  onClip: (clip: [number, number] | null) => void;
}

interface Props extends ViewProps {
  paused: boolean;
  onPause: (paused: boolean) => void;
}

const statusText = (s: Span) => (s.status === "running" ? "(pending)" : s.status);

function sizeText(s: Span) {
  const tokens = (s.tokens_in ?? 0) + (s.tokens_out ?? 0);
  return tokens ? `${fmtCount(tokens)} tok` : fmtBytes(payloadBytes(s));
}

export function NetworkView({ trace, tree, now, selected, onSelect, clip, onClip, paused, onPause }: Props) {
  const [query, setQuery] = useState("");
  const [invert, setInvert] = useState(false);
  const [groups, setGroups] = useState<Set<string>>(new Set());
  const [sort, setSort] = useState<{ col: Col; dir: 1 | -1 }>({ col: "waterfall", dir: 1 });

  const agents = useMemo(() => new Map(tree.spans.map((s) => [s.span_id, agentOf(s, tree)])), [tree]);
  const depth = useMemo(() => new Map(tree.spans.map((s) => [s.span_id, ancestors(s, tree).length])), [tree]);
  const match = useMemo(() => compileFilter(query), [query]);

  const t0 = tree.spans.length ? tree.spans[0].start : trace.start;
  const t1 = Math.max(t0 + 1, ...tree.spans.map((s) => endOf(s, now)));
  const [w0, w1] = clip ?? [t0, t1];
  const wRange = Math.max(w1 - w0, 1e-6);

  const kinds = groups.size ? new Set(KIND_GROUPS.filter((g) => groups.has(g.id)).flatMap((g) => g.kinds)) : null;
  const visible = tree.spans.filter(
    (s) => (!kinds || kinds.has(s.kind)) && match(s, agents.get(s.span_id)) !== invert && (!clip || overlaps(s, clip[0], clip[1], now)),
  );
  const key: Record<Col, (s: Span) => string | number> = {
    name: (s) => s.name.toLowerCase(),
    status: (s) => s.status,
    type: (s) => s.kind,
    initiator: (s) => agents.get(s.span_id) ?? "",
    size: (s) => (s.tokens_in ?? 0) + (s.tokens_out ?? 0) || payloadBytes(s) / 1e6,
    time: (s) => endOf(s, now) - s.start,
    waterfall: (s) => s.start,
  };
  const rows =
    sort.col === "waterfall" && sort.dir === 1
      ? visible
      : [...visible].sort((a, b) => {
          const [x, y] = [key[sort.col](a), key[sort.col](b)];
          return (x < y ? -1 : x > y ? 1 : 0) * sort.dir;
        });
  const indent = sort.col === "waterfall";

  const step = niceStep(wRange, 5);
  const ticks: number[] = [];
  for (let v = Math.ceil((w0 - t0) / step) * step; t0 + v <= w1; v += step) ticks.push(v);
  const pct = (t: number) => ((t - w0) / wRange) * 100;

  const toggleGroup = (id: string | null, e: MouseEvent) => {
    if (id === null) return setGroups(new Set());
    if (e.ctrlKey || e.metaKey) {
      const next = new Set(groups);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      setGroups(next);
    } else setGroups(new Set([id]));
  };

  const onKey = (e: KeyboardEvent) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    e.preventDefault();
    const i = rows.findIndex((s) => s.span_id === selected);
    const next = rows[Math.min(Math.max(i + (e.key === "ArrowDown" ? 1 : -1), 0), rows.length - 1)];
    if (next) onSelect(next.span_id);
  };

  const exportClip = () => {
    const blob = new Blob([JSON.stringify({ trace, clip, filter: query || undefined, spans: visible }, null, 2)], { type: "application/json" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = `${trace.name.replace(/[^\w-]+/g, "_").slice(0, 40)}${clip ? `_${Math.round(clip[0] - t0)}-${Math.round(clip[1] - t0)}ms` : ""}.json`;
    a.click();
    URL.revokeObjectURL(a.href);
  };

  const span = selected ? tree.byId.get(selected) : undefined;
  const tokens = (list: Span[]) => list.reduce((n, s) => n + (s.tokens_in ?? 0) + (s.tokens_out ?? 0), 0);
  const columns = span ? COLUMNS.slice(0, 1) : COLUMNS;
  const grid = span ? "1fr" : GRID;

  return (
    <div className="network">
      <div className="toolbar">
        <button className={paused ? "record" : "record on"} title={paused ? "Resume live updates" : "Pause live updates"} onClick={() => onPause(!paused)}>
          <span className="dot" />
          {paused ? "Paused" : "Live"}
        </button>
        <button className="tool-btn" title="Export visible spans as JSON" onClick={exportClip}>
          ⭳ Export
        </button>
        <span className="sep" />
        {clip ? (
          <span className="clip-chip">
            Clip {fmtDuration(clip[0] - t0)} – {fmtDuration(clip[1] - t0)} ({fmtDuration(clip[1] - clip[0])})
            <button onClick={() => onClip(null)} title="Clear clip">
              ✕
            </button>
          </span>
        ) : (
          <span className="hint">Drag on the timeline to clip</span>
        )}
      </div>
      <div className="filterbar">
        <input className="filter" placeholder="Filter   agent:  kind:  status:  model:  -exclude" value={query} onChange={(e) => setQuery(e.target.value)} />
        <label className="check">
          <input type="checkbox" checked={invert} onChange={(e) => setInvert(e.target.checked)} /> Invert
        </label>
        <span className="sep" />
        <button className={groups.size === 0 ? "chip active" : "chip"} onClick={(e) => toggleGroup(null, e)}>
          All
        </button>
        {KIND_GROUPS.map((g) => (
          <button key={g.id} className={groups.has(g.id) ? "chip active" : "chip"} onClick={(e) => toggleGroup(g.id, e)} title="Ctrl-click to select several">
            {g.label}
          </button>
        ))}
      </div>
      <Overview spans={tree.spans} t0={t0} t1={t1} now={now} clip={clip} onClip={onClip} />
      <div className={span ? "table-area with-details" : "table-area"}>
        <div className="table" tabIndex={0} onKeyDown={onKey}>
          <div className="thead" style={{ gridTemplateColumns: grid }}>
            {columns.map((c) => (
              <div
                key={c.id}
                className={sort.col === c.id ? "th sorted" : "th"}
                onClick={() => setSort({ col: c.id, dir: sort.col === c.id ? (-sort.dir as 1 | -1) : 1 })}
              >
                {c.label}
                {sort.col === c.id && <span className="arrow">{sort.dir === 1 ? "▲" : "▼"}</span>}
                {c.id === "waterfall" &&
                  ticks.map((v) => (
                    <span key={v} className="tick-label" style={{ left: `${pct(t0 + v)}%` }}>
                      {fmtDuration(v)}
                    </span>
                  ))}
              </div>
            ))}
          </div>
          <div className="tbody">
            {!span && (
              <div className="grid-overlay" style={{ gridTemplateColumns: grid }}>
                <div className="ticks" style={{ gridColumn: 7 }}>
                  {ticks.map((v) => (
                    <div key={v} className="tick" style={{ left: `${pct(t0 + v)}%` }} />
                  ))}
                </div>
              </div>
            )}
            {rows.map((s) => {
              const agent = agents.get(s.span_id);
              const parent = s.parent_id ? tree.byId.get(s.parent_id) : undefined;
              const end = endOf(s, now);
              const left = pct(s.start);
              return (
                <div
                  key={s.span_id}
                  className={`tr ${s.span_id === selected ? "selected" : ""} ${s.status === "error" ? "failed" : ""}`}
                  style={{ gridTemplateColumns: grid }}
                  onClick={() => onSelect(s.span_id)}
                >
                  <div className="td name" style={{ paddingLeft: 6 + (indent ? (depth.get(s.span_id) ?? 0) * 12 : 0) }} title={s.name}>
                    <KindIcon kind={s.kind} />
                    <span>{s.name}</span>
                  </div>
                  {!span && (
                    <>
                      <div className={`td status-${s.status}`}>{statusText(s)}</div>
                      <div className="td">{s.kind}</div>
                      <div className="td" title={parent ? `called by ${parent.name}` : undefined}>
                        {agent && <span style={{ color: agentColor(agent) }}>● </span>}
                        {agent ?? ""}
                        {parent && <span className="muted"> › {parent.name}</span>}
                      </div>
                      <div className="td num">{sizeText(s)}</div>
                      <div className="td num">{fmtDuration(end - s.start)}</div>
                      <div className="td waterfall" title={`Started at ${fmtDuration(s.start - t0)} · ${fmtDuration(end - s.start)}`}>
                        <div
                          className={`bar kind-${s.kind} ${s.status}`}
                          style={{ left: `${Math.max(left, 0)}%`, width: `${Math.max(Math.min(pct(end), 100) - Math.max(left, 0), 0.4)}%` }}
                        />
                      </div>
                    </>
                  )}
                </div>
              );
            })}
            {!rows.length && <div className="no-rows">No spans match the current filters.</div>}
          </div>
        </div>
        {span && <SpanDetails span={span} tree={tree} now={now} onSelect={onSelect} onClose={() => onSelect(null)} />}
      </div>
      <div className="statusbar">
        <span>
          {visible.length} / {tree.spans.length} spans
        </span>
        <span>
          {fmtCount(tokens(visible))} / {fmtCount(tokens(tree.spans))} tokens
        </span>
        <span>{new Set([...agents.values()].filter(Boolean)).size} agents</span>
        {trace.errors > 0 && <span className="status-error">{trace.errors} errors</span>}
        <span>Finish: {fmtDuration(t1 - t0)}</span>
      </div>
    </div>
  );
}
