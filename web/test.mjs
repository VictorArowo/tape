// Checks the browser build against the native engine: every strategy,
// under several execution settings, must produce identical run hashes and
// statistics in WebAssembly (loaded through tape-engine.js, as a page
// would) and natively (`tape web run`). Run from the repository root:
//
//   node web/test.mjs
//
// after ./web/build.sh.

import { execFileSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import { createEngine } from "./dist/tape-engine.js";

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

timings.sort((a, b) => a - b);
console.log(`\n${cases.length} runs, median ${timings[timings.length >> 1].toFixed(0)} ms per run (two backtests each)`);
if (failures) {
  console.log(`${failures} mismatches`);
  process.exit(1);
}
console.log("WebAssembly and native results are identical");
