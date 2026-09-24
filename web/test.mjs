// Checks the browser build against the native engine: every strategy,
// under several execution settings, must produce identical run hashes and
// statistics in WebAssembly (loaded through tape-engine.js, as a page
// would) and natively (`tape web run`). Then checks custom strategies: the
// crossover rewritten in JavaScript must match the native one exactly, the
// examples must run, and mistakes in a strategy must be explained. Run
// from the repository root:
//
//   node web/test.mjs
//
// after ./web/build.sh.

import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createEngine } from "./dist/tape-engine.js";
import { examples } from "./dist/strategies.js";

const dist = new URL("./dist/", import.meta.url);

// Node's fetch doesn't read files, so serve the build directory by hand.
const fetchFile = async (url) => {
  try {
    const bytes = await readFile(new URL(url));
    return { ok: true, status: 200, arrayBuffer: async () => bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) };
  } catch {
    return { ok: false, status: 404 };
  }
};

const engine = await createEngine({ base: dist.href, fetch: fetchFile });

const realistic = { latencyMs: 100, feeBps: 10, slippageBps: 2, participationPct: 10, fillOnTouch: false };
const cases = [];
for (const [strategy, extra] of [
  ["sma", { fast: 20, slow: 100 }],
  ["meanrev", { window: 60, band: 2 }],
  ["breakout", { lookback: 240 }],
  ["hold", {}],
]) {
  for (const execution of [
    realistic,
    { ...realistic, fillOnTouch: true, participationPct: 100 },
    { latencyMs: 0, feeBps: 0, slippageBps: 0, participationPct: 100, fillOnTouch: true },
    { latencyMs: 750, feeBps: 25, slippageBps: 12.5, participationPct: 1, fillOnTouch: false },
  ]) {
    cases.push({ strategy, ...extra, month: strategy === "meanrev" ? "2024-03" : "2024-01", execution });
  }
}

let failures = 0;
const timings = [];
for (const params of cases) {
  const web = await engine.run(params);
  timings.push(web.elapsedMs);
  const native = JSON.parse(
    execFileSync("./target/release/tape", [
      "web", "run",
      "--data", new URL(`data/btcusdt-${params.month}.bin`, dist).pathname,
      "--params", JSON.stringify(params),
    ]).toString(),
  );
  const same =
    web.runHash === native.runHash &&
    web.naiveRunHash === native.naiveRunHash &&
    JSON.stringify(web.stats) === JSON.stringify(native.stats) &&
    JSON.stringify(web.equity) === JSON.stringify(native.equity);
  const label = `${params.strategy.padEnd(8)} ${params.month} fee ${String(params.execution.feeBps).padStart(4)} bps touch ${params.execution.fillOnTouch ? "y" : "n"}`;
  console.log(
    `${same ? "same" : "DIFFERENT"}  ${label}  return ${(web.stats.totalReturn * 100).toFixed(2).padStart(7)}%  naive ${(web.stats.naiveReturn * 100).toFixed(2).padStart(7)}%  trades ${String(web.stats.trades).padStart(5)}  ${web.elapsedMs.toFixed(0).padStart(4)} ms  ${web.runHash}`,
  );
  if (!same) failures += 1;
}

// Bad input is reported, not thrown away.
try {
  await engine.run({ ...cases[0], slow: 5 });
  console.log("DIFFERENT  expected an error for fast >= slow");
  failures += 1;
} catch (err) {
  console.log(`error ok  ${err.message}`);
}

// The native crossover, in JavaScript: exact sums of closes in cents, and
// all-in sizing at the bar's close.
const cross = `
const FAST = 20, SLOW = 100;
const cents = [];
function onBar(bar, ctx) {
  cents.push(Math.round(bar.close * 100));
  const n = cents.length;
  if (n < SLOW || ctx.openOrders > 0) return;
  const sum = (len) => cents.slice(n - len).reduce((a, b) => a + b, 0);
  const wantLong = sum(FAST) * SLOW > sum(SLOW) * FAST;
  if (wantLong && ctx.position === 0) {
    const qty = ctx.maxBuy(bar.close);
    if (qty > 0) ctx.buy(qty);
  } else if (!wantLong && ctx.position > 0) {
    ctx.sell(ctx.position);
  }
}`;

console.log("");
for (const params of cases.filter((c) => c.strategy === "sma")) {
  const native = await engine.run(params);
  const custom = await engine.run({ ...params, strategy: "custom", code: cross });
  const same =
    custom.runHash === native.runHash &&
    custom.naiveRunHash === native.naiveRunHash &&
    JSON.stringify(custom.stats) === JSON.stringify(native.stats);
  console.log(`${same ? "same" : "DIFFERENT"}  custom crossover vs native, fee ${String(params.execution.feeBps).padStart(4)} bps  ${custom.elapsedMs.toFixed(0).padStart(4)} ms  ${custom.runHash}`);
  if (!same) failures += 1;
}

for (const example of examples) {
  for (const month of ["2024-01", "2024-03"]) {
    try {
      const r = await engine.run({ strategy: "custom", code: example.code, month, execution: realistic });
      const again = await engine.run({ strategy: "custom", code: example.code, month, execution: realistic });
      const ok = r.stats.trades > 0 && again.runHash === r.runHash;
      console.log(`${ok ? "runs" : "DIFFERENT"}  ${example.id.padEnd(9)} ${month}  return ${(r.stats.totalReturn * 100).toFixed(2).padStart(7)}%  naive ${(r.stats.naiveReturn * 100).toFixed(2).padStart(7)}%  trades ${String(r.stats.trades).padStart(5)}  log ${r.log.length}  ${r.elapsedMs.toFixed(0).padStart(4)} ms`);
      if (!ok) failures += 1;
    } catch (err) {
      console.log(`FAILED  ${example.id} ${month}: ${err.message}`);
      failures += 1;
    }
  }
}

// Mistakes, and what the page is told about them.
const base = { strategy: "custom", month: "2024-01", execution: realistic };
const mistakes = [
  ["syntax error", "function onBar(bar, ctx) {", /SyntaxError/],
  ["no onBar", "function onbar() {}", /define a function onBar/],
  ["throws", "let n = 0;\nfunction onBar(bar, ctx) {\n  n += 1;\n  if (n === 500) null.price;\n}", /TypeError: .*null.*\(line 4\), in onBar at 2024-01-01T08:20:00Z/],
  ["bad quantity", "function onBar(bar, ctx) { ctx.buy(-1); }", /positive quantity/],
  ["limit and stop", "function onBar(bar, ctx) { ctx.buy(0.1, { limit: 1, stop: 2 }); }", /not both/],
  ["clock", "function onBar(bar, ctx) { Date.now(); }", /no clock/],
  ["network", "function onBar(bar, ctx) { fetch('https://api.binance.com'); }", /fetch is not a function/],
  ["top level", "throw new Error('nope');\nfunction onBar() {}", /nope/],
];
for (const [label, code, expected] of mistakes) {
  try {
    await engine.run({ ...base, code });
    console.log(`DIFFERENT  expected an error for ${label}`);
    failures += 1;
  } catch (err) {
    const ok = expected.test(err.message) && err.name === "StrategyError";
    console.log(`${ok ? "error ok" : "DIFFERENT"}  ${label.padEnd(14)} ${err.message}`);
    if (!ok) failures += 1;
  }
}

// Seeded randomness and logging: the same code gives the same run.
const coin = "function onBar(bar, ctx) {\n  if (ctx.openOrders > 0) return;\n  if (Math.random() < 0.01) ctx.position > 0 ? ctx.sell(ctx.position) : ctx.buy(ctx.maxBuy());\n}\nfunction onFill(fill, ctx) { console.log(fill.side, fill.qty, 'at', fill.price); }";
const flips = [await engine.run({ ...base, code: coin }), await engine.run({ ...base, code: coin })];
const seeded = flips[0].runHash === flips[1].runHash && flips[0].stats.trades > 10 && flips[0].log.length === flips[0].stats.trades && /^(buy|sell) [\d.]+ at [\d.]+$/.test(flips[0].log[0]);
console.log(`${seeded ? "same" : "DIFFERENT"}  random strategy twice: ${flips[0].stats.trades} trades, log "${flips[0].log[0]}"`);
if (!seeded) failures += 1;
// A built-in run after a custom one isn't affected by it.
const after = await engine.run(cases[0]);
if (after.log.length !== 0 || after.strategy === "custom") {
  console.log("DIFFERENT  built-in run after custom runs");
  failures += 1;
}

timings.sort((a, b) => a - b);
console.log(`\n${cases.length} built-in runs, median ${timings[timings.length >> 1].toFixed(0)} ms per run (two backtests each)`);
if (failures) {
  console.log(`${failures} mismatches`);
  process.exit(1);
}
console.log("WebAssembly and native results are identical");
