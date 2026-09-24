// The in-browser backtest: tape's engine compiled to WebAssembly, run on a
// month of real BTC/USDT one-minute bars. Each run is two backtests: the
// settings on the form, and naive execution for comparison.
//
// Every run goes through a Web Worker, so a visitor's own strategy can't
// freeze the page, and one that runs too long is stopped. Custom code only
// ever comes from the editor and only runs when the visitor presses Run.

import { lineChart } from "./charts.js";
import { createWorkerEngine } from "./engine/tape-engine.js";
import { examples } from "./engine/strategies.js";
import { colors, fmt } from "./format.js";

const PRESETS = {
  naive: { latency: 0, fees: 0, slippage: 0, participation: 100, touch: true },
  realistic: { latency: 100, fees: 10, slippage: 2, participation: 10, touch: false },
};
const CODE_KEY = "tape.demo.code";

const form = document.getElementById("demo-form");
const output = document.querySelector('[data-demo="output"]');
const tiles = document.querySelector('[data-demo="tiles"]');
const meta = document.querySelector('[data-demo="meta"]');
const hash = document.querySelector('[data-demo="hash"]');
const stopButton = document.querySelector('[data-demo="stop"]');
const runButton = form.querySelector('button[type="submit"]');
const codeError = document.querySelector('[data-demo="code-error"]');
const consolePanel = document.querySelector('[data-demo="console"]');
const logOutput = document.querySelector('[data-demo="log"]');
const code = form.elements.code;
const gutter = form.querySelector(".editor-gutter");

const engine = createWorkerEngine({ base: "./engine/", timeoutMs: 10_000 });
let running = false;

function readParams() {
  const f = new FormData(form);
  const num = (k) => Number(f.get(k));
  const params = {
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
  if (params.strategy === "custom") params.code = code.value;
  return params;
}

// --- Form ----------------------------------------------------------------------

function syncStrategyFields() {
  const strategy = form.elements.strategy.value;
  for (const field of form.querySelectorAll("[data-for]")) {
    field.hidden = !field.dataset.for.split(" ").includes(strategy);
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

// --- Code editor -----------------------------------------------------------------

// Per-visitor convenience only: storage can be unavailable or cleared.
function loadSavedCode() {
  try {
    return localStorage.getItem(CODE_KEY);
  } catch {
    return null;
  }
}

function saveCode() {
  try {
    localStorage.setItem(CODE_KEY, code.value);
  } catch {
    // Not saving is fine.
  }
}

let errorLine = null;

function renderGutter() {
  const lines = code.value.split("\n").length;
  const numbers = [];
  for (let i = 1; i <= lines; i++) {
    const span = document.createElement("span");
    span.textContent = i;
    if (i === errorLine) span.className = "is-error";
    numbers.push(span);
  }
  gutter.replaceChildren(...numbers);
  gutter.scrollTop = code.scrollTop;
}

function showCodeError(message, line) {
  errorLine = line ?? null;
  codeError.textContent = message;
  codeError.hidden = false;
  renderGutter();
  if (errorLine) {
    // Bring the line into view without moving the visitor's cursor.
    const lineHeight = parseFloat(getComputedStyle(code).lineHeight) || 20;
    const target = (errorLine - 1) * lineHeight;
    if (target < code.scrollTop || target > code.scrollTop + code.clientHeight - lineHeight) {
      code.scrollTop = Math.max(0, target - code.clientHeight / 3);
    }
  }
}

function clearCodeError() {
  if (codeError.hidden && errorLine === null) return;
  errorLine = null;
  codeError.hidden = true;
  codeError.textContent = "";
  renderGutter();
}

function loadExample(id) {
  const example = examples.find((e) => e.id === id);
  if (!example) return;
  code.value = example.code;
  code.scrollTop = 0;
  clearCodeError();
  saveCode();
  renderGutter();
}

// Tab indents; Esc then Tab leaves the editor, so keyboard users aren't trapped.
let escapePressed = false;
code.addEventListener("keydown", (e) => {
  if (e.key === "Escape") {
    escapePressed = true;
    return;
  }
  if (e.key === "Tab" && !escapePressed && !e.metaKey && !e.ctrlKey && !e.altKey) {
    e.preventDefault();
    const { selectionStart: start, selectionEnd: end, value } = code;
    if (e.shiftKey) {
      const lineStart = value.lastIndexOf("\n", start - 1) + 1;
      if (value.startsWith("  ", lineStart)) {
        code.setRangeText("", lineStart, lineStart + 2, "preserve");
        code.setSelectionRange(Math.max(lineStart, start - 2), Math.max(lineStart, end - 2));
      }
    } else {
      code.setRangeText("  ", start, end, "end");
    }
    code.dispatchEvent(new Event("input", { bubbles: true }));
    return;
  }
  escapePressed = false;
});
code.addEventListener("scroll", () => {
  gutter.scrollTop = code.scrollTop;
});

function initEditor() {
  const picker = form.elements.example;
  picker.replaceChildren(
    ...examples.map((e) => {
      const option = document.createElement("option");
      option.value = e.id;
      option.textContent = e.name;
      return option;
    }),
  );
  const saved = loadSavedCode();
  code.value = saved ?? examples[0]?.code ?? "";
  const match = examples.find((e) => e.code === code.value);
  if (match) picker.value = match.id;
  renderGutter();
}

// --- Running --------------------------------------------------------------------

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

function setRunning(on) {
  running = on;
  runButton.disabled = on;
  stopButton.hidden = !on;
  output.classList.toggle("is-running", on);
}

function showMetaError(message) {
  meta.classList.add("error");
  meta.textContent = message;
}

async function run() {
  if (running) return;
  const params = readParams();
  const custom = params.strategy === "custom";
  meta.classList.remove("error");
  meta.textContent = "Running…";
  if (custom) clearCodeError();
  setRunning(true);

  let result;
  try {
    result = await engine.run(params);
  } catch (err) {
    setRunning(false);
    // Don't leave the last run's results up next to an error about this one.
    output.hidden = true;
    const message = err instanceof Error ? capitalize(err.message) : "Something went wrong running the backtest.";
    if (err?.message === "stopped") {
      meta.textContent = "Stopped.";
    } else if (custom && err?.name === "StrategyError") {
      meta.textContent = "Your strategy stopped with an error.";
      showCodeError(message, err.line);
    } else {
      showMetaError(message);
    }
    return;
  }
  setRunning(false);
  output.hidden = false;

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
  const matchesNative = !custom && form.elements.preset.value === "realistic";
  hash.textContent =
    `Run hash ${result.runHash}. Run it again with the same settings and you'll get the same hash.` +
    (matchesNative ? " With the Realistic preset, it matches tape's native engine exactly." : "");

  consolePanel.hidden = !custom;
  if (custom) {
    const lines = result.log ?? [];
    logOutput.textContent = lines.length
      ? lines.join("\n")
      : "Nothing was logged. Call ctx.log(…) or console.log(…) in your strategy to print here.";
    logOutput.classList.toggle("is-empty", !lines.length);
  }
}

function capitalize(text) {
  return text.charAt(0).toUpperCase() + text.slice(1).replace(/([^.])$/, "$1.");
}

// --- Wiring ----------------------------------------------------------------------

form.addEventListener("input", (e) => {
  const name = e.target.name;
  if (name === "strategy") syncStrategyFields();
  if (name === "preset") applyPreset(e.target.value);
  if (["latency", "fees", "slippage", "participation", "touch"].includes(name)) {
    form.elements.preset.value = "custom";
    syncOutputs();
  }
  if (name === "code") {
    saveCode();
    clearCodeError();
    renderGutter();
    const match = examples.find((ex) => ex.code === code.value);
    if (match) form.elements.example.value = match.id;
  }
});

// Remember what the picker showed, so a cancelled switch can put it back.
let shownExample = null;
form.elements.example.addEventListener("focus", (e) => {
  shownExample = e.target.value;
});
form.elements.example.addEventListener("change", (e) => {
  const edited = !examples.some((ex) => ex.code === code.value);
  if (edited && !window.confirm("Replace your code with this example? Your changes will be lost.")) {
    if (shownExample) e.target.value = shownExample;
    return;
  }
  loadExample(e.target.value);
  shownExample = e.target.value;
});

form.addEventListener("submit", (e) => {
  e.preventDefault();
  run();
});

stopButton.addEventListener("click", () => engine.stop());

initEditor();
syncStrategyFields();
syncOutputs();
