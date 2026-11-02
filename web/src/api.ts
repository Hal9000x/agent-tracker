import { useEffect, useRef, useState } from "react";
import type { Span, TraceSummary } from "./model";

export type Connection = "connecting" | "live" | "offline";

interface Update {
  spans: Span[];
  traces: TraceSummary[];
  deleted?: string[];
}

/** Keeps whichever copy has the higher server seq, so fetches and stream events can race safely. */
function mergeBySeq<T extends { seq: number }>(prev: Map<string, T>, items: T[], key: (t: T) => string): Map<string, T> {
  let next: Map<string, T> | null = null;
  for (const item of items) {
    const k = key(item);
    const cur = (next ?? prev).get(k);
    if (!cur || cur.seq <= item.seq) {
      next ??= new Map(prev);
      next.set(k, item);
    }
  }
  return next ?? prev;
}

/**
 * Live trace list plus the spans of the selected trace. While `paused`,
 * stream updates queue up and are applied on resume.
 */
export function useTracker(selected: string | null, paused: boolean) {
  const [traces, setTraces] = useState<Map<string, TraceSummary>>(new Map());
  const [spans, setSpans] = useState<Map<string, Span>>(new Map());
  const [conn, setConn] = useState<Connection>("connecting");
  const [reload, setReload] = useState(0);
  const selectedRef = useRef(selected);
  selectedRef.current = selected;
  const pausedRef = useRef(paused);
  pausedRef.current = paused;
  const queue = useRef<Required<Update>>({ spans: [], traces: [], deleted: [] });
  const frame = useRef(0);

  const flush = () => {
    frame.current = 0;
    const { spans: ss, traces: ts, deleted } = queue.current;
    queue.current = { spans: [], traces: [], deleted: [] };
    if (ts.length) setTraces((prev) => mergeBySeq(prev, ts, (t) => t.trace_id));
    if (deleted.length)
      setTraces((prev) => {
        const next = new Map(prev);
        for (const id of deleted) next.delete(id);
        return next;
      });
    const mine = ss.filter((s) => s.trace_id === selectedRef.current);
    if (mine.length) setSpans((prev) => mergeBySeq(prev, mine, (s) => s.span_id));
  };
  const schedule = () => {
    if (!frame.current && !pausedRef.current) frame.current = requestAnimationFrame(flush);
  };

  useEffect(() => {
    // The fetched list is the truth (traces deleted meanwhile drop out), except
    // where the stream already delivered something newer.
    const loadTraces = () =>
      fetch("/api/traces")
        .then((r) => r.json())
        .then((list: TraceSummary[]) =>
          setTraces((prev) => {
            const fresh = new Map(list.map((t) => [t.trace_id, t]));
            for (const [id, t] of prev) if ((fresh.get(id)?.seq ?? Infinity) < t.seq) fresh.set(id, t);
            return fresh;
          }),
        )
        .catch(() => {});
    const es = new EventSource("/api/stream");
    es.onopen = () => {
      setConn("live");
      loadTraces();
      setReload((n) => n + 1);
    };
    es.onerror = () => setConn("offline");
    es.addEventListener("update", (e) => {
      const u: Update = JSON.parse((e as MessageEvent).data);
      for (const s of u.spans) queue.current.spans.push(s); // a spread throws past ~130k arguments
      queue.current.traces.push(...u.traces);
      queue.current.deleted.push(...(u.deleted ?? []));
      schedule();
    });
    es.addEventListener("resync", () => {
      loadTraces();
      setReload((n) => n + 1);
    });
    return () => {
      es.close();
      cancelAnimationFrame(frame.current);
    };
  }, []);

  useEffect(() => {
    if (!paused) schedule();
  }, [paused]);

  useEffect(() => setSpans(new Map()), [selected]);

  useEffect(() => {
    if (!selected) return;
    let cancelled = false;
    fetch(`/api/traces/${encodeURIComponent(selected)}`)
      .then((r) => (r.ok ? r.json() : []))
      .then((list: Span[]) => {
        if (!cancelled) setSpans((prev) => mergeBySeq(prev, list, (s) => s.span_id));
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [selected, reload]);

  return { traces, spans, conn };
}

/** Whether this tracker needs a sign-in; a signed-in owner also gets the ingest token for hooks. */
export interface Session {
  auth: boolean;
  signed_in: boolean;
  ingest_token?: string;
}

/** The session, null until known, and a function that checks it again. */
export function useSession(): [Session | null, () => void] {
  const [session, setSession] = useState<Session | null>(null);
  const [checks, setChecks] = useState(0);
  useEffect(() => {
    fetch("/api/session")
      .then((r) => r.json())
      .then(setSession)
      .catch(() => setSession((s) => s ?? { auth: false, signed_in: true }));
  }, [checks]);
  return [session, () => setChecks((n) => n + 1)];
}

/** Resolves to an error message, or null once signed in. */
export async function signIn(username: string, password: string): Promise<string | null> {
  const r = await fetch("/api/login", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ username, password }) });
  return r.ok ? null : (await r.text()) || `sign-in failed (${r.status})`;
}

export function signOut(): Promise<void> {
  return fetch("/api/logout", { method: "POST", headers: { "content-type": "application/json" }, body: "{}" }).then(() => {});
}

/** Permanently removes a trace; every open view drops it via the live stream. */
export function deleteTrace(id: string): Promise<void> {
  return fetch(`/api/traces/${encodeURIComponent(id)}`, { method: "DELETE" }).then(() => {});
}

/** Asks the daemon to replay its built-in demo runs; they arrive over the live stream. */
export function playDemo(): void {
  fetch("/api/demo", { method: "POST", headers: { "content-type": "application/json" } }).catch(() => {});
}

/** Current time, ticking while `active` so running spans grow. */
export function useNow(active: boolean, intervalMs = 250): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!active) return;
    setNow(Date.now());
    const id = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [active, intervalMs]);
  return now;
}
