import { useEffect, useRef, useState } from "react";
import type { Span, TraceSummary } from "./model";

export type Connection = "connecting" | "live" | "offline";

interface Update {
  spans: Span[];
  traces: TraceSummary[];
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
  const queue = useRef<Update>({ spans: [], traces: [] });
  const frame = useRef(0);

  const flush = () => {
    frame.current = 0;
    const { spans: ss, traces: ts } = queue.current;
    queue.current = { spans: [], traces: [] };
    if (ts.length) setTraces((prev) => mergeBySeq(prev, ts, (t) => t.trace_id));
    const mine = ss.filter((s) => s.trace_id === selectedRef.current);
    if (mine.length) setSpans((prev) => mergeBySeq(prev, mine, (s) => s.span_id));
  };
  const schedule = () => {
    if (!frame.current && !pausedRef.current) frame.current = requestAnimationFrame(flush);
  };

  useEffect(() => {
    const loadTraces = () =>
      fetch("/api/traces")
        .then((r) => r.json())
        .then((list: TraceSummary[]) => setTraces((prev) => mergeBySeq(prev, list, (t) => t.trace_id)))
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
      queue.current.spans.push(...u.spans);
      queue.current.traces.push(...u.traces);
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
