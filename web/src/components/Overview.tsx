import { useEffect, useLayoutEffect, useRef, useState, type PointerEvent } from "react";
import { endOf, fmtDuration, KIND_COLOR, niceStep, packLanes, type Span } from "../model";

const HEIGHT = 64;
const TOP = 16;
const LANE = 4;
const LANES = Math.floor((HEIGHT - TOP - 2) / LANE);
const EDGE_PX = 6;

type Mode = "new" | "left" | "right" | "move";

interface Props {
  spans: Span[];
  t0: number;
  t1: number;
  now: number;
  clip: [number, number] | null;
  onClip: (clip: [number, number] | null) => void;
}

/** Whole-trace timeline; drag to clip a time range, drag its edges to resize, double-click to clear. */
export function Overview({ spans, t0, t1, now, clip, onClip }: Props) {
  const wrap = useRef<HTMLDivElement>(null);
  const canvas = useRef<HTMLCanvasElement>(null);
  const drag = useRef<{ mode: Mode; x0: number; a: number; b: number } | null>(null);
  const [width, setWidth] = useState(0);
  const range = Math.max(t1 - t0, 1);
  const toX = (t: number) => ((t - t0) / range) * width;
  const toT = (x: number) => t0 + (Math.min(Math.max(x, 0), width) / width) * range;

  useLayoutEffect(() => {
    const el = wrap.current!;
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const c = canvas.current;
    if (!c || !width) return;
    const dpr = window.devicePixelRatio || 1;
    c.width = width * dpr;
    c.height = HEIGHT * dpr;
    const ctx = c.getContext("2d")!;
    ctx.scale(dpr, dpr);
    ctx.clearRect(0, 0, width, HEIGHT);
    const step = niceStep(range, Math.max(width / 110, 2));
    ctx.font = "10px system-ui, sans-serif";
    for (let v = 0; v <= range; v += step) {
      const x = Math.round(toX(t0 + v)) + 0.5;
      ctx.fillStyle = "#3c4043";
      ctx.fillRect(x, 0, 1, HEIGHT);
      ctx.fillStyle = "#9aa0a6";
      ctx.fillText(fmtDuration(v), x + 3, 11);
    }
    const lanes = packLanes(spans, now);
    spans.forEach((s, i) => {
      const x = toX(s.start);
      const w = Math.max(toX(endOf(s, now)) - x, 1.5);
      ctx.fillStyle = s.status === "error" ? "#f28b82" : KIND_COLOR[s.kind];
      ctx.fillRect(x, TOP + (lanes[i] % LANES) * LANE, w, LANE - 1.5);
    });
  }, [spans, t0, t1, now, width]);

  const localX = (e: PointerEvent) => e.clientX - wrap.current!.getBoundingClientRect().left;

  const onPointerDown = (e: PointerEvent) => {
    if (e.button !== 0) return;
    const x = localX(e);
    let mode: Mode = "new";
    if (clip) {
      const [ax, bx] = [toX(clip[0]), toX(clip[1])];
      if (Math.abs(x - ax) <= EDGE_PX) mode = "left";
      else if (Math.abs(x - bx) <= EDGE_PX) mode = "right";
      else if (x > ax && x < bx) mode = "move";
    }
    drag.current = { mode, x0: x, a: clip?.[0] ?? toT(x), b: clip?.[1] ?? toT(x) };
    (e.target as Element).setPointerCapture(e.pointerId);
  };

  const onPointerMove = (e: PointerEvent) => {
    const d = drag.current;
    if (!d) return;
    const x = localX(e);
    if (d.mode === "new") {
      if (Math.abs(x - d.x0) >= 3) onClip([toT(Math.min(x, d.x0)), toT(Math.max(x, d.x0))]);
    } else if (d.mode === "left") onClip([Math.min(toT(x), d.b), d.b]);
    else if (d.mode === "right") onClip([d.a, Math.max(toT(x), d.a)]);
    else {
      const dt = Math.min(Math.max(toT(x) - toT(d.x0), t0 - d.a), t1 - d.b);
      onClip([d.a + dt, d.b + dt]);
    }
  };

  const onPointerUp = (e: PointerEvent) => {
    const d = drag.current;
    drag.current = null;
    if (d?.mode === "new" && Math.abs(localX(e) - d.x0) < 3) onClip(null);
  };

  const cursorAt = (x: number) => {
    if (!clip) return "crosshair";
    const [ax, bx] = [toX(clip[0]), toX(clip[1])];
    if (Math.abs(x - ax) <= EDGE_PX || Math.abs(x - bx) <= EDGE_PX) return "ew-resize";
    return x > ax && x < bx ? "grab" : "crosshair";
  };

  return (
    <div
      className="overview"
      ref={wrap}
      style={{ height: HEIGHT }}
      onPointerDown={onPointerDown}
      onPointerMove={(e) => {
        (e.currentTarget as HTMLElement).style.cursor = cursorAt(localX(e));
        onPointerMove(e);
      }}
      onPointerUp={onPointerUp}
      onDoubleClick={() => onClip(null)}
      title="Drag to clip a time range · double-click to clear"
    >
      <canvas ref={canvas} style={{ width: "100%", height: HEIGHT }} />
      {clip && width > 0 && (
        <>
          <div className="shade" style={{ left: 0, width: toX(clip[0]) }} />
          <div className="shade" style={{ left: toX(clip[1]), right: 0 }} />
          <div className="clip-box" style={{ left: toX(clip[0]), width: Math.max(toX(clip[1]) - toX(clip[0]), 1) }} />
        </>
      )}
    </div>
  );
}
