import { useState } from "react";

function Value({ value }: { value: unknown }) {
  if (value === null) return <span className="j-null">null</span>;
  switch (typeof value) {
    case "string":
      return <span className="j-str">"{value}"</span>;
    case "number":
      return <span className="j-num">{value}</span>;
    case "boolean":
      return <span className="j-bool">{String(value)}</span>;
    default:
      return <span>{String(value)}</span>;
  }
}

function Node({ name, value, depth }: { name?: string; value: unknown; depth: number }) {
  const isObj = value !== null && typeof value === "object";
  const [open, setOpen] = useState(depth < 2);
  const key = name !== undefined && <span className="j-key">{name}: </span>;
  if (!isObj) {
    return (
      <div className="j-row" style={{ paddingLeft: depth * 14 + 12 }}>
        {key}
        <Value value={value} />
      </div>
    );
  }
  const entries = Array.isArray(value) ? value.map((v, i) => [String(i), v] as const) : Object.entries(value as object);
  const summary = Array.isArray(value) ? `Array(${entries.length})` : `{${entries.slice(0, 3).map(([k]) => k).join(", ")}${entries.length > 3 ? ", …" : ""}}`;
  return (
    <>
      <div className="j-row j-toggle" style={{ paddingLeft: depth * 14 }} onClick={() => setOpen(!open)}>
        <span className="j-caret">{open ? "▾" : "▸"}</span>
        {key}
        <span className="j-summary">{summary}</span>
      </div>
      {open && entries.map(([k, v]) => <Node key={k} name={k} value={v} depth={depth + 1} />)}
    </>
  );
}

export function JsonTree({ value }: { value: unknown }) {
  return (
    <div className="json-tree">
      <Node value={value} depth={0} />
    </div>
  );
}
