// The in-browser backtest: tape's engine compiled to WebAssembly, run on a
// month of real BTC/USDT one-minute bars. Each run is two backtests: the
// settings on the form, and naive execution for comparison.

import { lineChart } from "./charts.js";
import { createEngine } from "./engine/tape-engine.js";
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

// --- Engine -------------------------------------------------------------------

let enginePromise = null;

// Loads the WebAssembly module once. A failed load can be retried.
function engine() {
  enginePromise ??= createEngine({ base: "./engine/" }).catch((err) => {
    enginePromise = null;
    throw err;
  });
  return enginePromise;
}

async function runBacktest(params) {
  return (await engine()).run(params);
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
  meta.classList.remove("error");
  meta.textContent = enginePromise ? "Running…" : "Loading the engine…";
  output.classList.add("is-running");
  let result;
  try {
    result = await runBacktest(params);
  } catch (err) {
    output.classList.remove("is-running");
    meta.classList.add("error");
    meta.textContent = err instanceof Error ? capitalize(err.message) : "Something went wrong running the backtest.";
    return;
  }
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

  meta.textContent = `Two backtests of ${fmt.count(result.events)} one-minute bars each, in ${fmt.count(Math.round(result.elapsedMs))} ms, in your browser.`;
  hash.textContent =
    `Run hash ${result.runHash}. Run it again with the same settings and you'll get the same hash.` +
    (form.elements.preset.value === "realistic" ? " With the Realistic preset, it matches tape's native engine exactly." : "");
}

function capitalize(text) {
  return text.charAt(0).toUpperCase() + text.slice(1).replace(/([^.])$/, "$1.");
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

// Start loading the engine and the default month as the demo scrolls near.
new IntersectionObserver(
  (entries, observer) => {
    if (!entries.some((e) => e.isIntersecting)) return;
    observer.disconnect();
    engine()
      .then((e) => e.preload(form.elements.month.value))
      .catch(() => {});
  },
  { rootMargin: "800px" },
).observe(form);

syncStrategyFields();
syncOutputs();
