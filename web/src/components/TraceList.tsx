import { useState } from "react";
import { fmtClock, fmtCount, fmtDuration, type TraceSummary } from "../model";

interface Props {
  traces: TraceSummary[];
  selected: string | null;
  onSelect: (id: string) => void;
}

export function TraceList({ traces, selected, onSelect }: Props) {
  const [query, setQuery] = useState("");
  const q = query.toLowerCase();
  const shown = q ? traces.filter((t) => [t.name, t.source ?? "", ...t.agents].some((f) => f.toLowerCase().includes(q))) : traces;
  return (
    <aside className="traces">
      <div className="traces-head">
        <input className="filter" placeholder="Search traces" value={query} onChange={(e) => setQuery(e.target.value)} />
      </div>
      <div className="traces-list">
        {shown.map((t) => (
          <button key={t.trace_id} className={t.trace_id === selected ? "trace active" : "trace"} onClick={() => onSelect(t.trace_id)}>
            <div className="trace-row">
              <span className={`dot status-${t.status}`} />
              <span className="trace-name" title={t.name}>
                {t.name}
              </span>
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
          </button>
        ))}
        {!shown.length && <div className="empty pad">{traces.length ? "No traces match." : "No traces yet."}</div>}
      </div>
    </aside>
  );
}
