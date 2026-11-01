// Sends three simulated agent runs to a running tracker, in real time.
//   node clients/typescript/examples/demo.ts [single|swarm|handoff]...
// DEMO_SPEED=4 runs four times faster.

import { Tracker, type Span } from "../tracker.ts";

const t = new Tracker();
const speed = Number(process.env.DEMO_SPEED ?? 1);
const sleep = (ms: number) => new Promise((r) => setTimeout(r, (ms * (0.75 + Math.random() / 2)) / speed));
const MODEL = "claude-sonnet-5-5";

async function single() {
  const prompt = "What's the weather in Lisbon tomorrow, and do I need an umbrella?";
  await t.trace("Weather in Lisbon tomorrow?", prompt, () =>
    t.agent("assistant", { input: prompt }, async () => {
      const call = await t.llm("plan", { model: MODEL, input: [{ role: "user", content: prompt }] }, async (s) => {
        await sleep(900);
        s.tokens(412, 58);
        return { tool_use: { name: "get_weather", input: { city: "Lisbon", day: "tomorrow" } } };
      });
      const weather = await t.tool("get_weather", { input: call.tool_use.input }, async () => {
        await sleep(600);
        return { forecast: "light rain", precip_mm: 4, high_c: 19, low_c: 13 };
      });
      return t.llm("answer", { model: MODEL, input: [{ role: "tool", content: weather }] }, async (s) => {
        await sleep(1100);
        s.tokens(530, 96);
        return "Light rain is expected tomorrow (about 4 mm), so yes: bring an umbrella.";
      });
    }),
  );
}

async function research(question: string, i: number) {
  return t.agent(`researcher-${i + 1}`, { input: question }, async () => {
    const hits = await t.tool("web_search", { input: { query: question } }, async () => {
      await sleep(500 + i * 200);
      return [{ url: `https://example.com/ssb-${i + 1}`, title: `Source ${i + 1} on ${question}` }];
    });
    if (i === 1) {
      await t.tool("fetch_page", { input: { url: hits[0].url } }, async () => {
        await sleep(400);
        throw new Error("HTTP 403 Forbidden");
      }).catch(() => {});
    }
    const page = await t.tool("fetch_page", { input: { url: hits[0].url }, attrs: { attempt: i === 1 ? 2 : 1 } }, async () => {
      await sleep(700);
      return { bytes: 18_240, excerpt: `Key facts about ${question.toLowerCase()}…` };
    });
    return t.llm("summarize", { model: MODEL, input: page }, async (s) => {
      await sleep(900 + i * 150);
      s.tokens(2100 + i * 300, 240);
      return `Finding ${i + 1}: ${question} — summarized from ${hits.length} source(s).`;
    });
  });
}

async function swarm() {
  const prompt = "Write a one-page market brief on solid-state batteries for EV makers.";
  await t.trace("Market brief: solid-state batteries", prompt, () =>
    t.agent("orchestrator", { input: prompt }, async () => {
      const questions = await t.llm("plan research", { model: MODEL, input: prompt }, async (s) => {
        await sleep(1000);
        s.tokens(380, 120);
        return ["Who are the leading manufacturers?", "What are current cost curves?", "Which EV makers have signed supply deals?"];
      });
      const findings = await t.split("split into research tasks", { input: questions }, () =>
        Promise.all(questions.map((q, i) => research(q, i))),
      );
      const merged = await t.join("merge findings", { input: findings }, async () => {
        await sleep(150);
        return findings.join("\n");
      });
      const draft = await t.agent("writer", { input: merged }, () =>
        t.llm("draft brief", { model: MODEL, input: merged }, async (s) => {
          await sleep(1600);
          s.tokens(3200, 850);
          return "DRAFT: Solid-state batteries are approaching pilot scale…";
        }),
      );
      let flagged!: (span: Span) => void;
      const flag = new Promise<Span>((resolve) => (flagged = resolve));
      const reviews = await t.fork("review draft", { input: draft }, () =>
        Promise.all([
          t.agent("fact-checker", { input: draft }, async () => {
            await t.tool("verify_claims", { input: { claims: 6 } }, async (s) => {
              await sleep(700);
              flagged(s);
              return { verified: 5, disputed: ["2027 cost parity claim"] };
            });
            return t.llm("verdict", { model: MODEL }, async (s) => {
              await sleep(600);
              s.tokens(900, 80);
              return "5/6 claims verified; cost-parity date is disputed.";
            });
          }),
          t.agent("critic", { input: draft }, async () => {
            const from = await flag;
            return t.llm("critique", { model: MODEL, links: [from], input: "draft + fact-checker flag" }, async (s) => {
              await sleep(1100);
              s.tokens(1400, 210);
              return "Tighten the intro; soften the 2027 cost-parity claim flagged by the fact-checker.";
            });
          }),
        ]),
      );
      await t.join("apply reviews", { input: reviews }, async () => sleep(120));
      return t.llm("final edit", { model: MODEL, input: { draft, reviews } }, async (s) => {
        await sleep(1300);
        s.tokens(4100, 900);
        return "FINAL: Solid-state batteries — market brief (1 page).";
      });
    }),
  );
}

async function handoff() {
  const prompt = "My March invoice charged me twice. Can you refund the duplicate?";
  await t.trace("Double charge on March invoice", prompt, async () => {
    const intent = await t.agent("triage", { input: prompt }, () =>
      t.llm("classify", { model: "claude-haiku-5-5", input: prompt }, async (s) => {
        await sleep(500);
        s.tokens(210, 24);
        return { intent: "billing.refund", route_to: "billing" };
      }),
    );
    await t.agent("billing", { input: intent }, async () => {
      const invoice = await t.tool("lookup_invoice", { input: { month: "2026-03" } }, async () => {
        await sleep(450);
        return { id: "INV-3301", charges: [49, 49], duplicate: true };
      });
      await t.tool("issue_refund", { input: { invoice: invoice.id, amount: 49 } }, async () => {
        await sleep(1500);
        throw new Error("payment provider timeout after 1500ms");
      }).catch(() => {});
      return t.llm("reply", { model: MODEL, input: { invoice, refund: "failed" } }, async (s) => {
        await sleep(800);
        s.tokens(640, 110);
        return "I found the duplicate charge. The refund didn't go through yet; I've escalated it and you'll hear back within 24h.";
      });
    });
  });
}

const scenarios: Record<string, () => Promise<void>> = { single, swarm, handoff };
const picked = process.argv.slice(2);
for (const name of picked.length ? picked : Object.keys(scenarios)) {
  if (!scenarios[name]) throw new Error(`unknown scenario ${name}; pick from ${Object.keys(scenarios).join(", ")}`);
  console.log(`running ${name}…`);
  await scenarios[name]();
}
await t.flush();
console.log(`done — open ${t.url}`);
