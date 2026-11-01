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
  type NodeProps,
  type ReactFlowInstance,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import { useMemo, useRef, useState, type CSSProperties } from "react";
import {
  agentColor,
  agentGraph,
  agentOf,
  ancestors,
  compileFilter,
  descendants,
  endOf,
  flowEdges,
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
const PAD = 14;
const LABEL = 20;

type Mode = "steps" | "agents";
type Stats = { llm: number; tool: number; tokens: number; errors: number };
type SpanData = { span: Span; agent?: string; now: number; dim: boolean; picked: boolean; stats?: Stats };
type GroupData = { label: string; color: string };
type SpanNode = Node<SpanData, "span" | "agent">;
type GroupNode = Node<GroupData, "agentGroup">;

function StepNode({ data }: NodeProps<SpanNode>) {
  const { span, agent, now, dim, picked } = data;
  return (
    <div className={`fnode status-${span.status}${dim ? " dim" : ""}${picked ? " picked" : ""}`} style={{ "--kind": KIND_COLOR[span.kind] } as CSSProperties}>
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
  const { span, agent, now, dim, picked, stats } = data;
  const title = span.kind === "agent" ? (agent ?? span.name) : span.name;
  const parts = stats && [stats.llm && `${stats.llm} llm`, stats.tool && `${stats.tool} tools`, stats.tokens && `${fmtCount(stats.tokens)} tok`].filter(Boolean);
  return (
    <div
      className={`fnode agent-node status-${span.status}${dim ? " dim" : ""}${picked ? " picked" : ""}`}
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

function layout(nodes: Span[], edges: FlowEdge[]): Map<string, { x: number; y: number }> {
  const g = new dagre.graphlib.Graph();
  g.setGraph({ rankdir: "LR", nodesep: 28, ranksep: 70, marginx: 24, marginy: 24 });
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

export function GraphView({ trace, tree, now, selected, onSelect, clip }: ViewProps) {
  const [mode, setMode] = useState<Mode>("steps");
  const [search, setSearch] = useState("");

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

  const members = (n: Span) => agents?.owned.get(n.span_id) ?? [n];
  const isDim = (n: Span) =>
    (lit !== null && !lit.has(n.span_id)) ||
    (match !== null && !members(n).some((s) => match(s, agentOf(s, tree)))) ||
    (clip !== null && !members(n).some((s) => overlaps(s, clip[0], clip[1], now)));

  const flowNodes: Node[] = nodes.map(
    (n): SpanNode => ({
      id: n.span_id,
      type: agents ? "agent" : "span",
      position: positions.get(n.span_id) ?? { x: 0, y: 0 },
      width: NODE_W,
      height: NODE_H,
      zIndex: 1000,
      data: {
        span: n,
        agent: agentOf(n, tree),
        now,
        dim: isDim(n),
        picked: n.span_id === focus,
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
      const x = Math.min(...pts.map((p) => p.x)) - PAD;
      const y = Math.min(...pts.map((p) => p.y)) - PAD - LABEL;
      const name = agentOf(a, tree) ?? a.name;
      flowNodes.push({
        id: `group:${a.span_id}`,
        type: "agentGroup",
        position: { x, y },
        width: Math.max(...pts.map((p) => p.x)) + NODE_W + PAD - x,
        height: Math.max(...pts.map((p) => p.y)) + NODE_H + PAD - y,
        zIndex: ancestors(a, tree).filter((s) => s.kind === "agent").length,
        selectable: false,
        focusable: false,
        data: { label: name, color: agentColor(name) },
      } satisfies GroupNode);
    }
  }

  const partLabel = (e: FlowEdge) => {
    const src = tree.byId.get(e.source);
    if (agents || src?.kind !== "split") return undefined;
    const kids = tree.children.get(src.span_id) ?? [];
    return `part ${kids.findIndex((k) => k.span_id === e.target) + 1}/${kids.length}`;
  };

  const flowEdgesRf: Edge[] = edges.map((e) => {
    const dim = lit !== null && (e.back || !(lit.has(e.source) && lit.has(e.target)));
    const color = e.link ? "#c58af9" : "#80868b";
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
      style: { stroke: color, strokeWidth: 1.5, ...(e.link ? { strokeDasharray: "6 4" } : {}) },
      zIndex: 999,
    };
  });

  const span = selected ? tree.byId.get(selected) : undefined;

  // Long runs are wide; fitting all of them makes nodes unreadable. Below a
  // readable zoom, start at the message on the left instead.
  const canvas = useRef<HTMLDivElement>(null);
  const initialView = (rf: ReactFlowInstance) => {
    const pts = [...positions.values()];
    if (!pts.length || !canvas.current) return;
    const [x0, x1] = [Math.min(...pts.map((p) => p.x)) - PAD, Math.max(...pts.map((p) => p.x)) + NODE_W + PAD];
    const [y0, y1] = [Math.min(...pts.map((p) => p.y)) - PAD - LABEL, Math.max(...pts.map((p) => p.y)) + NODE_H + PAD];
    const { clientWidth: w, clientHeight: h } = canvas.current;
    const fit = Math.min((w - 60) / (x1 - x0), (h - 60) / (y1 - y0));
    const zoom = Math.min(Math.max(fit, 0.6), 1);
    const x = fit >= 0.6 ? (w - (x1 - x0) * zoom) / 2 - x0 * zoom : 30 - x0 * zoom;
    rf.setViewport({ x, y: (h - (y1 - y0) * zoom) / 2 - y0 * zoom, zoom });
  };

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
        {clip && <span className="hint">Clip active: spans outside it are dimmed</span>}
      </div>
      <div className="graph-area">
        <div className="canvas" ref={canvas}>
          <ReactFlow
            key={`${trace.trace_id}:${mode}`}
            nodes={flowNodes}
            edges={flowEdgesRf}
            nodeTypes={nodeTypes}
            edgeTypes={edgeTypes}
            colorMode="dark"
            onInit={initialView}
            minZoom={0.05}
            nodesDraggable={false}
            nodesConnectable={false}
            onNodeClick={(_, n) => n.type !== "agentGroup" && onSelect(n.id)}
            onPaneClick={() => onSelect(null)}
          >
            <Background variant={BackgroundVariant.Dots} gap={18} size={1} color="#3c4043" />
            <MiniMap pannable zoomable nodeColor={(n) => (n.type === "agentGroup" ? "transparent" : KIND_COLOR[(n.data as SpanData).span.kind])} maskColor="#20212499" />
            <Controls showInteractive={false} />
          </ReactFlow>
        </div>
        {span && <SpanDetails span={span} tree={tree} now={now} onSelect={onSelect} onClose={() => onSelect(null)} />}
      </div>
    </div>
  );
}
