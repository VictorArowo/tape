// The in-browser backtest. `runBacktest` is the seam where tape's
// WebAssembly build plugs in. Until then it returns placeholder results
// (flagged on the page) with the same shape the engine will produce.

import { lineChart } from "./charts.js";
import { colors, fmt } from "./format.js";

const PRESETS = {
  naive: { latency: 0, fees: 0, slippage: 0, participation: 100, touch: true },
  realistic: { latency: 100, fees: 10, slippage: 2, participation: 10, touch: false },
};

const form = document.getElementById("demo-form");
const output = document.querySelector('[data-demo="output"]');
const tiles = document.querySelector('[data-demo="tiles"]');
const meta = document.querySelector('[data-demo="meta"]');
const hash = document.querySelector('[data-demo="hash"]');
const flag = document.querySelector("[data-demo-flag]");

function readParams() {
  const f = new FormData(form);
  const num = (k) => Number(f.get(k));
  return {
    strategy: f.get("strategy"),
    fast: num("fast"),
    slow: num("slow"),
    window: num("window"),
    band: num("band"),
    lookback: num("lookback"),
    month: f.get("month"),
    execution: {
      latencyMs: num("latency"),
      feeBps: num("fees"),
      slippageBps: num("slippage"),
      participationPct: num("participation"),
      fillOnTouch: f.get("touch") === "on",
    },
  };
}

// --- Placeholder engine -----------------------------------------------------

function hashString(s) {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619);
  return h >>> 0;
}

function rng(seed) {
  let a = seed || 1;
  return () => {
    a ^= a << 13;
    a ^= a >>> 17;
    a ^= a << 5;
    return ((a >>> 0) % 1_000_000) / 1_000_000;
  };
}

function placeholderRun(params) {
  const signal = JSON.stringify({ ...params, execution: undefined });
  const rand = rng(hashString(signal));
  const hours = 24 * 30;
  const tradesPerDay = params.strategy === "hold" ? 0 : Math.max(1, Math.round(240 / Math.max(5, params.fast || params.window || params.lookback / 4)));
  const e = params.execution;
  const costPerTrade = (e.feeBps + e.slippageBps) / 10_000 + e.latencyMs / 1_000_000;
  const cap = e.participationPct / 100;
  let naive = 10_000;
  let honest = 10_000;
  const naiveSeries = [naive];
  const honestSeries = [honest];
  let fees = 0;
  for (let h = 1; h < hours; h++) {
    const move = (rand() - 0.48) * 0.006;
    naive *= 1 + move;
    const trades = tradesPerDay / 24;
    const cost = honest * trades * costPerTrade * 0.5;
    fees += honest * trades * (e.feeBps / 10_000) * 0.5;
    honest = honest * (1 + move * (0.85 + 0.15 * cap)) - cost;
    naiveSeries.push(naive);
    honestSeries.push(honest);
  }
  const [y, m] = params.month.split("-").map(Number);
  const dates = honestSeries.map((_, i) => new Date(Date.UTC(y, m - 1, 1, i)).toISOString());
  let peak = honestSeries[0];
  let maxDd = 0;
  for (const v of honestSeries) {
    peak = Math.max(peak, v);
    maxDd = Math.min(maxDd, v / peak - 1);
  }
  return {
    placeholder: true,
    events: 44_640,
    elapsedMs: 18 + (hashString(JSON.stringify(params)) % 40),
    runHash: hashString(JSON.stringify(params)).toString(16).padStart(8, "0") + hashString(signal).toString(16).padStart(8, "0"),
    dates,
    equity: honestSeries,
    naiveEquity: naiveSeries,
    stats: {
      totalReturn: honest / 10_000 - 1,
      naiveReturn: naive / 10_000 - 1,
      maxDrawdown: maxDd,
      trades: tradesPerDay * 30,
      feesPaid: fees,
    },
  };
}

// Flip to false once runBacktest calls the WebAssembly engine.
const ENGINE_IS_PLACEHOLDER = true;

async function runBacktest(params) {
  return placeholderRun(params);
}

// --- Page wiring -------------------------------------------------------------

function syncStrategyFields() {
  const strategy = form.elements.strategy.value;
  for (const field of form.querySelectorAll("[data-for]")) {
    field.hidden = field.dataset.for !== strategy;
  }
}

function syncOutputs() {
  for (const slider of form.querySelectorAll(".slider")) {
    const input = slider.querySelector("input");
    const out = slider.querySelector("output");
    out.textContent = `${input.value} ${out.dataset.unit}`;
  }
}

function applyPreset(name) {
  const preset = PRESETS[name];
  if (!preset) return;
  form.elements.latency.value = preset.latency;
  form.elements.fees.value = preset.fees;
  form.elements.slippage.value = preset.slippage;
  form.elements.participation.value = preset.participation;
  form.elements.touch.checked = preset.touch;
  syncOutputs();
}

function tile(label, value, note) {
  const div = document.createElement("div");
  div.className = "tile";
  const l = document.createElement("span");
  l.className = "tile-label";
  l.textContent = label;
  const v = document.createElement("span");
  v.className = "tile-value";
  v.textContent = value;
  div.append(l, v);
  if (note) {
    const n = document.createElement("span");
    n.className = "tile-note";
    n.textContent = note;
    div.appendChild(n);
  }
  return div;
}

async function run() {
  const params = readParams();
  meta.textContent = "Running…";
  output.classList.add("is-running");
  const result = await runBacktest(params);
  flag.hidden = !result.placeholder;
  output.hidden = false;
  output.classList.remove("is-running");

  const s = result.stats;
  tiles.replaceChildren(
    tile("Return", fmt.percent(s.totalReturn), `${fmt.percent(s.naiveReturn)} if naive`),
    tile("Worst drawdown", fmt.percent(s.maxDrawdown)),
    tile("Trades", fmt.count(s.trades)),
    tile("Fees paid", fmt.money(s.feesPaid)),
  );

  const step = Math.max(1, Math.floor(result.dates.length / 240));
  const idx = result.dates.map((_, i) => i).filter((i) => i % step === 0 || i === result.dates.length - 1);
  lineChart(document.getElementById("demo-chart"), {
    x: idx.map((i) => result.dates[i]),
    series: [
      { key: "naive", label: "Naive backtest", color: colors.naive, values: idx.map((i) => result.naiveEquity[i]) },
      { key: "tape", label: "Your settings", color: colors.tape, values: idx.map((i) => result.equity[i]) },
    ],
    yFormat: (v, exact) => (exact ? fmt.moneyExact(v) : fmt.money(v)),
    xFormat: (iso, long) => {
      const d = new Date(iso);
      const day = d.getUTCDate();
      const mon = d.toLocaleString("en-US", { month: "short", timeZone: "UTC" });
      return long ? `${day} ${mon}, ${String(d.getUTCHours()).padStart(2, "0")}:00 UTC` : `${day} ${mon}`;
    },
    height: 300,
    ariaLabel: `Equity over the month. Your settings end at ${fmt.money(result.equity.at(-1))}; a naive backtest ends at ${fmt.money(result.naiveEquity.at(-1))}.`,
  });

  meta.textContent = `${fmt.count(result.events)} events in ${result.elapsedMs} ms, in your browser.`;
  hash.textContent = `Run hash ${result.runHash}. Run it again with the same settings and you'll get the same hash.`;
}

form.addEventListener("input", (e) => {
  const name = e.target.name;
  if (name === "strategy") syncStrategyFields();
  if (name === "preset") applyPreset(e.target.value);
  if (["latency", "fees", "slippage", "participation", "touch"].includes(name)) {
    form.elements.preset.value = "custom";
    syncOutputs();
  }
});

form.addEventListener("submit", (e) => {
  e.preventDefault();
  run();
});

flag.hidden = !ENGINE_IS_PLACEHOLDER;
syncStrategyFields();
syncOutputs();
