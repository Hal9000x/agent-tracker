import { Component, useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react";
import { deleteTrace, playDemo, signIn, signOut, useNow, useSession, useTracker, type Session } from "./api";
import { ConnectGuide } from "./components/ConnectGuide";
import { GraphView } from "./components/GraphView";
import { NetworkView } from "./components/NetworkView";
import { TraceList } from "./components/TraceList";
import { buildTree, type TraceSummary } from "./model";

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
  const [session, recheck] = useSession();
  if (!session) return null;
  if (!session.signed_in) return <SignIn onSignedIn={recheck} />;
  return <Tracker session={session} recheck={recheck} />;
}

function SignIn({ onSignedIn }: { onSignedIn: () => void }) {
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const submit = (e: FormEvent<HTMLFormElement>) => {
    e.preventDefault();
    const form = new FormData(e.currentTarget);
    setBusy(true);
    signIn(String(form.get("username")), String(form.get("password"))).then((err) => {
      setBusy(false);
      if (err) setError(err);
      else onSignedIn();
    });
  };
  return (
    <div className="sign-in">
      <form onSubmit={submit}>
        <span className="brand">
          <span className="logo" /> Agent Tracker
        </span>
        <label>
          Username
          <input name="username" autoComplete="username" required autoFocus />
        </label>
        <label>
          Password
          <input name="password" type="password" autoComplete="current-password" required />
        </label>
        {error && <div className="error-banner">{error}</div>}
        <button className="primary-btn" disabled={busy}>
          Sign in
        </button>
      </form>
    </div>
  );
}

function Tracker({ session, recheck }: { session: Session; recheck: () => void }) {
  const [route, navigate] = useRoute();
  const [paused, setPaused] = useState(false);
  const [selected, setSelected] = useState<string | null>(null);
  const [clip, setClip] = useState<[number, number] | null>(null);
  const [connecting, setConnecting] = useState(false);
  const { traces, spans, conn } = useTracker(route.traceId, paused);

  // A lost connection may be an expired sign-in; checking brings back the sign-in page if so.
  useEffect(() => {
    if (conn === "offline" && session.auth) recheck();
  }, [conn]);

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
        <button className="tool-btn" title="Set up Claude Code, OpenTelemetry or your own agents" onClick={() => setConnecting(true)}>
          ⚡ Connect
        </button>
        <button className="tool-btn" title="Replay three sample agent runs" onClick={playDemo}>
          ▶ Demo
        </button>
        <span className={`conn ${conn}`} title="Connection to the tracker daemon">
          <span className="dot" />
          {conn}
        </span>
        {session.auth && (
          <button className="tool-btn" title="Sign out of this tracker" onClick={() => signOut().then(recheck)}>
            Sign out
          </button>
        )}
      </header>
      <div className="body">
        <TraceList
          traces={list}
          selected={route.traceId}
          onSelect={(id) => navigate({ ...route, traceId: id })}
          onDelete={(t) => {
            if (!confirm(`Delete "${t.name}"? This cannot be undone.`)) return;
            const next = list.find((o) => o.trace_id !== t.trace_id);
            deleteTrace(t.trace_id).then(() => t.trace_id === route.traceId && navigate({ ...route, traceId: next?.trace_id ?? null }));
          }}
        />
        <main>
          {viewProps ? (
            <ViewBoundary retry={`${route.view}:${route.traceId}`}>
              {route.view === "network" ? <NetworkView {...viewProps} paused={paused} onPause={setPaused} /> : <GraphView {...viewProps} />}
            </ViewBoundary>
          ) : (
            <EmptyState traces={list} session={session} />
          )}
        </main>
      </div>
      {connecting && (
        <div className="modal-backdrop" onClick={() => setConnecting(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <h2>Connect a harness</h2>
              <button className="close" title="Close" onClick={() => setConnecting(false)}>
                ✕
              </button>
            </div>
            <ConnectGuide traces={list} session={session} />
          </div>
        </div>
      )}
    </div>
  );
}

interface BoundaryProps {
  /** Changing it (another view or trace) clears the error and renders again. */
  retry: string;
  children: ReactNode;
}

/** Keeps a crash in one view from blanking the whole app. */
class ViewBoundary extends Component<BoundaryProps, { error: string | null; retry: string }> {
  state = { error: null as string | null, retry: this.props.retry };

  static getDerivedStateFromError(e: unknown) {
    return { error: e instanceof Error ? e.message : String(e) };
  }

  static getDerivedStateFromProps(props: BoundaryProps, state: { retry: string }) {
    return props.retry === state.retry ? null : { error: null, retry: props.retry };
  }

  render() {
    if (this.state.error === null) return this.props.children;
    return (
      <div className="empty center">
        <p>This view failed to render. Pick another trace or view.</p>
        <div className="error-banner">{this.state.error}</div>
      </div>
    );
  }
}

function EmptyState({ traces, session }: { traces: TraceSummary[]; session: Session }) {
  if (traces.length) return <div className="empty center">Select a trace.</div>;
  return (
    <div className="empty center onboarding">
      <h2>Waiting for agent traces</h2>
      <p>Connect a harness below and its runs show up here live, or try the demo first.</p>
      <button className="primary-btn" onClick={playDemo}>
        ▶ Play demo runs
      </button>
      <p className="muted">A single agent, a research swarm and a support handoff, replayed in real time.</p>
      <ConnectGuide traces={traces} session={session} />
    </div>
  );
}
