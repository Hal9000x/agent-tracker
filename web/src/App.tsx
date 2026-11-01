import { useEffect, useMemo, useState } from "react";
import { useNow, useTracker } from "./api";
import { GraphView } from "./components/GraphView";
import { NetworkView } from "./components/NetworkView";
import { TraceList } from "./components/TraceList";
import { buildTree } from "./model";

type View = "network" | "graph";

interface Route {
  view: View;
  traceId: string | null;
}

function parseHash(): Route {
  const m = location.hash.match(/^#\/(network|graph)(?:\/(.+))?$/);
  return { view: (m?.[1] as View) ?? "network", traceId: m?.[2] ? decodeURIComponent(m[2]) : null };
}

function useRoute(): [Route, (r: Route) => void] {
  const [route, setRoute] = useState(parseHash);
  useEffect(() => {
    const onHash = () => setRoute(parseHash());
    addEventListener("hashchange", onHash);
    return () => removeEventListener("hashchange", onHash);
  }, []);
  const navigate = (r: Route) => {
    location.hash = `#/${r.view}${r.traceId ? `/${encodeURIComponent(r.traceId)}` : ""}`;
  };
  return [route, navigate];
}

export function App() {
  const [route, navigate] = useRoute();
  const [paused, setPaused] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [clip, setClip] = useState<[number, number] | null>(null);
  const { traces, spans, conn } = useTracker(route.traceId, paused);

  const list = useMemo(() => [...traces.values()].sort((a, b) => b.start - a.start), [traces]);
  const tree = useMemo(() => buildTree(spans.values()), [spans]);
  const trace = route.traceId ? traces.get(route.traceId) : undefined;
  const now = useNow(tree.spans.some((s) => s.status === "running"));

  useEffect(() => {
    setSelected(null);
    setClip(null);
  }, [route.traceId]);

  useEffect(() => {
    if (!route.traceId && list.length) navigate({ ...route, traceId: list[0].trace_id });
  }, [route.traceId, list.length]);

  const viewProps = trace && { trace, tree, now, selected, onSelect: setSelected, clip, onClip: setClip };

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">
          <span className="logo" /> Agent Tracker
        </span>
        <nav className="panels">
          {(["network", "graph"] as const).map((v) => (
            <button key={v} className={route.view === v ? "panel active" : "panel"} onClick={() => navigate({ ...route, view: v })}>
              {v === "network" ? "Network" : "Graph"}
            </button>
          ))}
        </nav>
        <span className="grow" />
        {trace && <span className="trace-title">{trace.name}</span>}
        <span className={`conn ${conn}`} title="Connection to the tracker daemon">
          <span className="dot" />
          {conn}
        </span>
      </header>
      <div className="body">
        <TraceList traces={list} selected={route.traceId} onSelect={(id) => navigate({ ...route, traceId: id })} />
        <main>
          {viewProps ? (
            route.view === "network" ? (
              <NetworkView {...viewProps} paused={paused} onPause={setPaused} />
            ) : (
              <GraphView {...viewProps} />
            )
          ) : (
            <EmptyState hasTraces={list.length > 0} />
          )}
        </main>
      </div>
    </div>
  );
}

function EmptyState({ hasTraces }: { hasTraces: boolean }) {
  if (hasTraces) return <div className="empty center">Select a trace.</div>;
  return (
    <div className="empty center onboarding">
      <h2>Waiting for agent traces</h2>
      <p>Send something to this tracker and it shows up here live.</p>
      <h3>Try the demo</h3>
      <pre>node clients/typescript/examples/demo.ts</pre>
      <h3>Claude Code</h3>
      <p>
        Add the hooks from <code>integrations/claude-code/settings.json</code> to your Claude Code settings.
      </p>
      <h3>OpenTelemetry</h3>
      <pre>OTEL_EXPORTER_OTLP_TRACES_ENDPOINT={location.origin}/v1/traces</pre>
    </div>
  );
}
