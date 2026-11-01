// Turns a tracker log (data/spans.jsonl) into the demo recording the daemon
// replays from POST /api/demo. Times become offsets from the first event and
// trace ids become short stable names.
//
// To regenerate: run a daemon with an empty --data dir, run
// `node clients/typescript/examples/demo.ts` against it, stop it, then
//   node scripts/record-demo.mjs <data-dir>/spans.jsonl daemon/demo/recording.jsonl

import { readFileSync, writeFileSync } from "node:fs";

const [input, output] = process.argv.slice(2);
if (!input || !output) {
  console.error("usage: node scripts/record-demo.mjs <spans.jsonl> <recording.jsonl>");
  process.exit(2);
}

const entries = readFileSync(input, "utf8").trim().split("\n").map((l) => JSON.parse(l));
const t0 = Math.min(...entries.flatMap(({ t, span }) => [t, span.start ?? t]));
const traceNames = new Map();

const lines = entries.map(({ t, span }) => {
  if (!traceNames.has(span.trace_id)) traceNames.set(span.trace_id, `t${traceNames.size + 1}`);
  const rel = (v) => (v === undefined ? undefined : Math.round((v - t0) * 1000) / 1000);
  return JSON.stringify({ t: rel(t), span: { ...span, trace_id: traceNames.get(span.trace_id), start: rel(span.start), end: rel(span.end) } });
});

writeFileSync(output, lines.join("\n") + "\n");
console.log(`${lines.length} updates, ${traceNames.size} traces, ${(Math.max(...entries.map((e) => e.t)) - t0).toFixed(0)} ms -> ${output}`);
