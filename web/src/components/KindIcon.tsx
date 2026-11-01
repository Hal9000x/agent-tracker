import { KIND_COLOR, type Kind } from "../model";

const PATHS: Record<Kind, { d: string; fill?: boolean }> = {
  message: { d: "M1.5 3h9v6.5h-9z M1.5 3.2 6 6.8l4.5-3.6" },
  agent: { d: "M6 1 10.5 3.5v5L6 11 1.5 8.5v-5z M4.5 6h3" },
  llm: { d: "M6 .8l1.3 3.9L11.2 6 7.3 7.3 6 11.2 4.7 7.3.8 6l3.9-1.3z", fill: true },
  tool: { d: "M8.6 1.2a2.6 2.6 0 0 0-2.4 3.5L1.4 9.5l1.1 1.1 4.8-4.8a2.6 2.6 0 0 0 3.5-2.4L9.3 4.9 7.1 2.7z", fill: true },
  split: { d: "M1 6h3.5M4.5 6 10 2M4.5 6H10M4.5 6 10 10" },
  fork: { d: "M1 6h3.5M4.5 6 10 2.5M4.5 6 10 9.5" },
  join: { d: "M2 2l4.5 4M2 10l4.5-4M6.5 6H11" },
  step: { d: "M6 3.5a2.5 2.5 0 1 0 0 5a2.5 2.5 0 1 0 0-5", fill: true },
};

export function KindIcon({ kind, size = 12 }: { kind: Kind; size?: number }) {
  const { d, fill } = PATHS[kind];
  const color = KIND_COLOR[kind];
  return (
    <svg width={size} height={size} viewBox="0 0 12 12" className="kind-icon" aria-label={kind}>
      <path d={d} fill={fill ? color : "none"} stroke={color} strokeWidth={fill ? 0 : 1.3} strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  );
}
