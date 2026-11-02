import dagre from "@dagrejs/dagre";
import {
  Background,
  BackgroundVariant,
  BaseEdge,
  Controls,
  Handle,
  MarkerType,
  MiniMap,
  Position,
  ReactFlow,
  type Edge,
  type EdgeProps,
  type Node,
  type NodeHandle,
  type NodeProps,
  type ReactFlowInstance,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { createContext, useContext, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import {
  agentColor,
  agentGraph,
  agentOf,
  ancestors,
  compileFilter,
  descendants,
  endOf,
  flowEdges,
  flowRanks,
  fmtCount,
  fmtDuration,
  KIND_COLOR,
  lineage,
  overlaps,
  preview,
  type FlowEdge,
  type Span,
} from "../model";
import { KindIcon } from "./KindIcon";
import type { ViewProps } from "./NetworkView";
import { SpanDetails } from "./SpanDetails";

const NODE_W = 230;
const NODE_H = 88;
const NODE_SEP = 28;
const RANK_SEP = 70;
const MARGIN = 24;
const PAD = 14;
const LABEL = 20;
// dagre recurses along long paths: ~160 ms at 1,000 chained nodes, stack
// overflow near 1,800. Bigger graphs get the linear layered layout.
const DAGRE_MAX_NODES = 1000;
const ACCENT = "#8ab4f8";
// The minimap draws every node and never culls: about 2.3 s to open 20k nodes.
const MINIMAP_MAX_NODES = 2000;
const HANDLE = 10; // .react-flow__handle in styles.css: 8px plus a 1px border
// Declared so React Flow knows every node's handles without rendering it first to
// measure them, which made onlyRenderVisibleElements render the whole graph once.
const SPAN_HANDLES: NodeHandle[] = [
  { type: "target", position: Position.Left, x: -HANDLE / 2, y: NODE_H / 2 - HANDLE / 2, width: HANDLE, height: HANDLE },
  { type: "source", position: Position.Right, x: NODE_W - HANDLE / 2, y: NODE_H / 2 - HANDLE / 2, width: HANDLE, height: HANDLE },
];

type Mode = "steps" | "agents";
type Point = { x: number; y: number };
type Stats = { llm: number; tool: number; tokens: number; errors: number };
/** `picked` is the selected step; `path` marks the steps upstream and downstream of it. */
type SpanData = { span: Span; agent?: string; dim: boolean; picked: boolean; path: boolean; stats?: Stats };
type GroupData = { label: string; color: string };
type SpanNode = Node<SpanData, "span" | "agent">;
type GroupNode = Node<GroupData, "agentGroup">;

// The ticking clock reaches nodes through context, so a tick re-renders the few
// nodes on screen instead of handing React Flow a new object for every node.
const Now = createContext(0);

const nodeClass = ({ span, dim, picked, path }: SpanData) =>
  `fnode status-${span.status}${dim ? " dim" : ""}${picked ? " picked" : ""}${path ? " on-path" : ""}`;

function StepNode({ data }: NodeProps<SpanNode>) {
  const { span, agent } = data;
  const now = useContext(Now);
  return (
    <div className={nodeClass(data)} style={{ "--kind": KIND_COLOR[span.kind] } as CSSProperties}>
      <Handle type="target" position={Position.Left} isConnectable={false} />
      <div className="fnode-head">
        <KindIcon kind={span.kind} />
        <span>{span.kind}</span>
        <span className="grow" />
        <span>{fmtDuration(endOf(span, now) - span.start)}</span>
      </div>
      <div className="fnode-title" title={span.name}>
        {span.name}
      </div>
      <div className="fnode-meta">
        {agent && <span style={{ color: agentColor(agent) }}>● {agent}</span>}
        {span.model && <span className="muted">{span.model}</span>}
      </div>
      <div className="fnode-io">{span.error ? <span className="status-error">✕ {span.error}</span> : preview(span.output ?? span.input, 64)}</div>
      <Handle type="source" position={Position.Right} isConnectable={false} />
    </div>
  );
}

function AgentNode({ data }: NodeProps<SpanNode>) {
  const { span, agent, stats } = data;
  const now = useContext(Now);
  const title = span.kind === "agent" ? (agent ?? span.name) : span.name;
  const parts = stats && [stats.llm && `${stats.llm} llm`, stats.tool && `${stats.tool} tools`, stats.tokens && `${fmtCount(stats.tokens)} tok`].filter(Boolean);
  return (
    <div
      className={`${nodeClass(data)} agent-node`}
      style={{ "--kind": span.kind === "agent" ? agentColor(agent) : KIND_COLOR[span.kind] } as CSSProperties}
    >
      <Handle type="target" position={Position.Left} isConnectable={false} />
      <div className="fnode-head">
        <KindIcon kind={span.kind} />
        <span>{span.kind}</span>
        <span className="grow" />
        <span>{fmtDuration(endOf(span, now) - span.start)}</span>
      </div>
      <div className="fnode-title" title={title}>
        {title}
      </div>
      {span.kind === "agent" && span.name !== title && <div className="fnode-meta muted">{span.name}</div>}
      <div className="fnode-io">
        {parts?.join(" · ")}
        {!!stats?.errors && <span className="status-error"> · {stats.errors} failed</span>}
      </div>
      <Handle type="source" position={Position.Right} isConnectable={false} />
    </div>
  );
}

function GroupBox({ data }: NodeProps<GroupNode>) {
  return (
    <div className="group-box" style={{ borderColor: `${data.color}88`, background: `${data.color}0d` }}>
      <span className="group-label" style={{ color: data.color }}>
        {data.label}
      </span>
    </div>
  );
}

/** An edge pointing back to an earlier node (work returning to an agent), arced above the graph. */
function BackEdge({ id, sourceX, sourceY, targetX, targetY, markerEnd, style, label }: EdgeProps) {
  const lift = Math.max(70, Math.abs(sourceX - targetX) * 0.12);
  const top = Math.min(sourceY, targetY) - lift;
  const path = `M ${sourceX},${sourceY} C ${sourceX + 80},${top} ${targetX - 80},${top} ${targetX},${targetY}`;
  const labelY = 0.75 * top + 0.125 * (sourceY + targetY);
  return <BaseEdge id={id} path={path} markerEnd={markerEnd} style={style} label={label} labelX={(sourceX + targetX) / 2} labelY={labelY} />;
}

const nodeTypes = { span: StepNode, agent: AgentNode, agentGroup: GroupBox };
const edgeTypes = { back: BackEdge };

function dagreLayout(nodes: Span[], edges: FlowEdge[]): Map<string, Point> {
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: "LR", nodesep: NODE_SEP, ranksep: RANK_SEP, marginx: MARGIN, marginy: MARGIN });
  g.setDefaultEdgeLabel(() => ({}));
  for (const n of nodes) g.setNode(n.span_id, { width: NODE_W, height: NODE_H });
  for (const e of edges) g.setEdge(e.source, e.target);
  dagre.layout(g);
  return new Map(
    nodes.map((n) => {
      const p = g.node(n.span_id);
      return [n.span_id, { x: p.x - NODE_W / 2, y: p.y - NODE_H / 2 }];
    }),
  );
}

/** Column = longest path from the roots, row = order within the column. */
function layeredLayout(nodes: Span[], edges: FlowEdge[]): Map<string, Point> {
  const rank = flowRanks(nodes, edges);
  const rows = new Map<number, number>();
  return new Map(
    nodes.map((n) => {
      const r = rank.get(n.span_id)!;
      const i = rows.get(r) ?? 0;
      rows.set(r, i + 1);
      return [n.span_id, { x: MARGIN + r * (NODE_W + RANK_SEP), y: MARGIN + i * (NODE_H + NODE_SEP) }];
    }),
  );
}

function layout(nodes: Span[], edges: FlowEdge[]): Map<string, Point> {
  if (nodes.length <= DAGRE_MAX_NODES) {
    try {
      return dagreLayout(nodes, edges);
    } catch (e) {
      console.warn("dagre layout failed, using the layered layout", e);
    }
  }
  return layeredLayout(nodes, edges);
}

function bounds(pts: Iterable<Point>) {
  let [x0, y0, x1, y1] = [Infinity, Infinity, -Infinity, -Infinity];
  for (const p of pts) {
    x0 = Math.min(x0, p.x);
    y0 = Math.min(y0, p.y);
    x1 = Math.max(x1, p.x);
    y1 = Math.max(y1, p.y);
  }
  return { x0, y0, x1, y1 };
}

// Long runs are wide; fitting all of them makes nodes unreadable. Below a
// readable zoom, start at the message on the left instead.
function startViewport(positions: Map<string, Point>, w: number, h: number) {
  const b = bounds(positions.values());
  const [x0, x1] = [b.x0 - PAD, b.x1 + NODE_W + PAD];
  const [y0, y1] = [b.y0 - PAD - LABEL, b.y1 + NODE_H + PAD];
  const fit = Math.min((w - 60) / (x1 - x0), (h - 60) / (y1 - y0));
  const zoom = Math.min(Math.max(fit, 0.6), 1);
  const x = fit >= 0.6 ? (w - (x1 - x0) * zoom) / 2 - x0 * zoom : 30 - x0 * zoom;
  return { x, y: (h - (y1 - y0) * zoom) / 2 - y0 * zoom, zoom };
}

export function GraphView({ trace, tree, now, selected, onSelect, clip }: ViewProps) {
  const [mode, setMode] = useState<Mode>("steps");
  const [search, setSearch] = useState("");
  const [focusPath, setFocusPath] = useState(false);
  const [drawerOpen, setDrawerOpen] = useState(true);

  const steps = useMemo(() => flowEdges(tree), [tree]);
  const agents = useMemo(() => (mode === "agents" ? agentGraph(tree, steps) : null), [mode, tree, steps]);
  const nodes = agents ? agents.nodes : tree.spans;
  const edges = agents ? agents.edges : steps;

  // Re-layout only when the graph's shape changes, not on every status update.
  const shape = `${mode}|${nodes.map((n) => n.span_id).join()}|${edges.map((e) => e.id).join()}`;
  const positions = useMemo(() => layout(nodes, edges), [shape]);

  const ownerOf = useMemo(() => {
    const m = new Map<string, string>();
    agents?.owned.forEach((list, owner) => list.forEach((s) => m.set(s.span_id, owner)));
    return m;
  }, [agents]);
  const focus = selected ? (ownerOf.get(selected) ?? selected) : null;
  const lit = useMemo(() => (focus ? lineage(focus, edges) : null), [focus, edges]);
  const match = useMemo(() => (search.trim() ? compileFilter(search) : null), [search]);

  // Only a clip makes dimming depend on the clock (running spans grow into it).
  const clipNow = clip ? now : 0;
  const flowNodes = useMemo(() => {
    const members = (n: Span) => agents?.owned.get(n.span_id) ?? [n];
    const isDim = (n: Span) =>
      (focusPath && lit !== null && !lit.has(n.span_id)) ||
      (match !== null && !members(n).some((s) => match(s, agentOf(s, tree)))) ||
      (clip !== null && !members(n).some((s) => overlaps(s, clip[0], clip[1], clipNow)));

    const list: Node[] = nodes.map(
      (n): SpanNode => ({
        id: n.span_id,
        type: agents ? "agent" : "span",
        position: positions.get(n.span_id) ?? { x: 0, y: 0 },
        width: NODE_W,
        height: NODE_H,
        measured: { width: NODE_W, height: NODE_H },
        handles: SPAN_HANDLES,
        zIndex: 1000,
        data: {
          span: n,
          agent: agentOf(n, tree),
          dim: isDim(n),
          picked: n.span_id === focus,
          path: lit !== null && n.span_id !== focus && lit.has(n.span_id),
          stats: agents
            ? members(n).reduce(
                (st, s) => ({
                  llm: st.llm + +(s.kind === "llm"),
                  tool: st.tool + +(s.kind === "tool"),
                  tokens: st.tokens + (s.tokens_in ?? 0) + (s.tokens_out ?? 0),
                  errors: st.errors + +(s.status === "error"),
                }),
                { llm: 0, tool: 0, tokens: 0, errors: 0 },
              )
            : undefined,
        },
      }),
    );

    if (!agents) {
      for (const a of tree.spans.filter((s) => s.kind === "agent")) {
        const pts = [a, ...descendants(a, tree)].map((s) => positions.get(s.span_id)).filter((p) => !!p);
        if (!pts.length) continue;
        const b = bounds(pts);
        const x = b.x0 - PAD;
        const y = b.y0 - PAD - LABEL;
        const name = agentOf(a, tree) ?? a.name;
        const [width, height] = [b.x1 + NODE_W + PAD - x, b.y1 + NODE_H + PAD - y];
        list.push({
          id: `group:${a.span_id}`,
          type: "agentGroup",
          position: { x, y },
          width,
          height,
          measured: { width, height },
          handles: [],
          zIndex: ancestors(a, tree).filter((s) => s.kind === "agent").length,
          selectable: false,
          focusable: false,
          data: { label: name, color: agentColor(name) },
        } satisfies GroupNode);
      }
    }
    return list;
  }, [nodes, positions, agents, tree, lit, match, focus, focusPath, clip, clipNow]);

  const flowEdgesRf = useMemo(() => {
    const partLabel = (e: FlowEdge) => {
      const src = tree.byId.get(e.source);
      if (agents || src?.kind !== "split") return undefined;
      const kids = tree.children.get(src.span_id) ?? [];
      return `part ${kids.findIndex((k) => k.span_id === e.target) + 1}/${kids.length}`;
    };

    return edges.map((e): Edge => {
      const onPath = lit !== null && !e.back && lit.has(e.source) && lit.has(e.target);
      const dim = focusPath && lit !== null && !onPath;
      const color = onPath ? ACCENT : e.link ? "#c58af9" : "#80868b";
      const back = (positions.get(e.target)?.x ?? 0) <= (positions.get(e.source)?.x ?? 0);
      return {
        id: e.id,
        source: e.source,
        target: e.target,
        type: back ? "back" : "default",
        animated: tree.byId.get(e.target)?.status === "running",
        label: e.count > 1 ? `×${e.count}` : e.link ? "message" : back ? "returns" : partLabel(e),
        className: dim ? "dim" : undefined,
        markerEnd: { type: MarkerType.ArrowClosed, color, width: 16, height: 16 },
        style: { stroke: color, strokeWidth: onPath ? 2.5 : 1.5, ...(e.link ? { strokeDasharray: "6 4" } : {}) },
        zIndex: 999,
      };
    });
  }, [edges, lit, positions, tree, agents, focusPath]);

  const span = selected ? tree.byId.get(selected) : undefined;

  const canvas = useRef<HTMLDivElement>(null);
  const flow = useRef<ReactFlowInstance | null>(null);
  const viewKey = `${trace.trace_id}:${mode}`;
  const [flowKey, setFlowKey] = useState(""); // viewKey of the ReactFlow that has initialized
  const started = useRef("");
  // After a trace switch the previous trace's spans render until this one's load.
  const ownPositions = positions.size > 0 && tree.spans[0]?.trace_id === trace.trace_id;
  useEffect(() => {
    if (flowKey !== viewKey || started.current === viewKey || !ownPositions || !flow.current || !canvas.current) return;
    started.current = viewKey;
    flow.current.setViewport(startViewport(positions, canvas.current.clientWidth, canvas.current.clientHeight));
  }, [flowKey, viewKey, ownPositions, positions]);

  // The details drawer floats over the canvas, so the whole diagram stays put;
  // a selected step outside the uncovered part is panned into it.
  const drawer = useRef<HTMLDivElement>(null);
  const drawerShown = !!span && drawerOpen;
  const [covered, setCovered] = useState(0);
  useLayoutEffect(() => {
    const el = drawer.current;
    if (!el) return setCovered(0);
    const ro = new ResizeObserver(() => setCovered(el.offsetWidth));
    ro.observe(el);
    return () => ro.disconnect();
  }, [drawerShown]);
  useEffect(() => {
    const rf = flow.current;
    const el = canvas.current;
    const p = focus ? positions.get(focus) : undefined;
    if (!rf || !el || !p || flowKey !== viewKey || !ownPositions) return;
    const { x, y, zoom } = rf.getViewport();
    const [left, top] = [p.x * zoom + x, p.y * zoom + y];
    const fits = left >= 16 && top >= 16 && left + NODE_W * zoom <= el.clientWidth - covered - 16 && top + NODE_H * zoom <= el.clientHeight - 16;
    if (!fits) rf.setCenter(p.x + NODE_W / 2 + covered / 2 / zoom, p.y + NODE_H / 2, { zoom, duration: 300 });
  }, [focus, flowKey, viewKey, ownPositions, covered]);

  return (
    <div className="graph">
      <div className="toolbar">
        <div className="segmented">
          {(["steps", "agents"] as const).map((m) => (
            <button key={m} className={mode === m ? "active" : ""} onClick={() => setMode(m)}>
              {m === "steps" ? "Steps" : "Agents"}
            </button>
          ))}
        </div>
        <input className="filter" placeholder="Highlight   agent:  kind:  status:  text" value={search} onChange={(e) => setSearch(e.target.value)} />
        <span className="sep" />
        <span className="legend">
          <span className="legend-line" /> flow
          <span className="legend-line link" /> message
        </span>
        <label className="check" title="Dim every step that is not upstream or downstream of the selected one">
          <input type="checkbox" checked={focusPath} onChange={(e) => setFocusPath(e.target.checked)} /> Focus path
        </label>
        {clip && <span className="hint">Clip active: spans outside it are dimmed</span>}
      </div>
      <div className="graph-area">
        <div className="canvas" ref={canvas}>
          <Now.Provider value={now}>
            <ReactFlow
              key={viewKey}
              nodes={flowNodes}
              edges={flowEdgesRf}
              nodeTypes={nodeTypes}
              edgeTypes={edgeTypes}
              colorMode="dark"
              onInit={(rf) => {
                flow.current = rf;
                setFlowKey(viewKey);
              }}
              onlyRenderVisibleElements
              minZoom={0.05}
              nodesDraggable={false}
              nodesConnectable={false}
              onNodeClick={(_, n) => n.type !== "agentGroup" && onSelect(n.id)}
              onPaneClick={() => onSelect(null)}
            >
              <Background variant={BackgroundVariant.Dots} gap={18} size={1} color="#3c4043" />
              {flowNodes.length <= MINIMAP_MAX_NODES && (
                <MiniMap
                  pannable
                  zoomable
                  style={{ marginRight: covered + 15 }}
                  nodeColor={(n) => (n.type === "agentGroup" ? "transparent" : n.id === focus ? ACCENT : KIND_COLOR[(n.data as SpanData).span.kind])}
                  maskColor="#20212499"
                />
              )}
              <Controls showInteractive={false} />
            </ReactFlow>
          </Now.Provider>
        </div>
        {span &&
          (drawerOpen ? (
            <div className="graph-drawer" ref={drawer}>
              <SpanDetails
                span={span}
                tree={tree}
                now={now}
                onSelect={onSelect}
                onClose={() => onSelect(null)}
                actions={
                  <button className="drawer-toggle" title="Hide details (keeps the selection)" onClick={() => setDrawerOpen(false)}>
                    »
                  </button>
                }
              />
            </div>
          ) : (
            <button className="drawer-tab" title="Show details" onClick={() => setDrawerOpen(true)}>
              « Details
            </button>
          ))}
      </div>
    </div>
  );
}
