import { useState } from "react";
import { fmtClock, fmtCount, fmtDuration, type TraceSummary } from "../model";

interface Props {
  traces: TraceSummary[];
  selected: string | null;
  onSelect: (id: string) => void;
  onDelete: (trace: TraceSummary) => void;
}

export function TraceList({ traces, selected, onSelect, onDelete }: Props) {
  const [query, setQuery] = useState("");
  const [source, setSource] = useState<string | null>(null);
  const sources = [...new Set(traces.map((t) => t.source).filter((s): s is string => !!s))].sort();
  const q = query.toLowerCase();
  const shown = traces.filter(
    (t) => (!source || t.source === source) && (!q || [t.name, t.source ?? "", ...t.agents].some((f) => f.toLowerCase().includes(q))),
  );
  return (
    <aside className="traces">
      <div className="traces-head">
        <input className="filter" placeholder="Search traces" value={query} onChange={(e) => setQuery(e.target.value)} />
        {sources.length > 1 && (
          <div className="source-chips">
            <button className={source === null ? "chip active" : "chip"} onClick={() => setSource(null)}>
              All
            </button>
            {sources.map((s) => (
              <button key={s} className={source === s ? "chip active" : "chip"} onClick={() => setSource(source === s ? null : s)}>
                {s}
              </button>
            ))}
          </div>
        )}
      </div>
      <div className="traces-list">
        {shown.map((t) => (
          <div
            key={t.trace_id}
            role="button"
            tabIndex={0}
            className={t.trace_id === selected ? "trace active" : "trace"}
            onClick={() => onSelect(t.trace_id)}
            onKeyDown={(e) => e.key === "Enter" && onSelect(t.trace_id)}
          >
            <div className="trace-row">
              <span className={`dot status-${t.status}`} />
              <span className="trace-name" title={t.name}>
                {t.name}
              </span>
              <button
                className="trace-delete"
                title="Delete this trace"
                onClick={(e) => {
                  e.stopPropagation();
                  onDelete(t);
                }}
              >
                ✕
              </button>
            </div>
            <div className="trace-meta">
              {fmtClock(t.start)} · {t.end !== null ? fmtDuration(t.end - t.start) : "running"} · {t.spans} spans
              {t.agents.length > 0 && ` · ${t.agents.length} agent${t.agents.length > 1 ? "s" : ""}`}
              {t.tokens > 0 && ` · ${fmtCount(t.tokens)} tok`}
            </div>
            <div className="trace-meta">
              {t.source && <span className="source">{t.source}</span>}
              {t.errors > 0 && <span className="status-error">{t.errors} failed</span>}
            </div>
          </div>
        ))}
        {!shown.length && <div className="empty pad">{traces.length ? "No traces match." : "No traces yet."}</div>}
      </div>
    </aside>
  );
}
