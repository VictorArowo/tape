// tape's backtesting engine, compiled to WebAssembly, for the browser.
//
//   import { createEngine } from "./engine/tape-engine.js";
//   const engine = await createEngine({ base: "./engine/" });
//   const result = await engine.run({ strategy: "sma", fast: 20, slow: 100,
//     month: "2024-01", execution: { latencyMs: 100, feeBps: 10,
//     slippageBps: 2, participationPct: 10, fillOnTouch: false } });
//
// `base` is where tape.wasm and data/ live. Months are fetched on first use
// and cached. A run happens synchronously inside the module; a month takes
// tens of milliseconds.
//
// Custom strategies. With `strategy: "custom"` and `code`, the engine runs
// your JavaScript instead of a built-in strategy:
//
//   engine.run({ strategy: "custom", code: `
//     function onBar(bar, ctx) {
//       if (ctx.position === 0) ctx.buy(ctx.maxBuy());
//     }`, month: "2024-01", execution })
//
// The code may define setup(ctx), onBar(bar, ctx), onFill(fill, ctx),
// onOrder(update, ctx) and onFinish(ctx); only onBar is required. Top-level
// variables are fresh for each backtest. See `ctx` below for what a
// strategy can read and do. Run untrusted or unfinished code through
// createWorkerEngine() instead, which runs it off the page and stops it if
// it doesn't finish.

const decoder = new TextDecoder();
const encoder = new TextEncoder();

// Codes from tape_core::abi.
const SIDE = { buy: 0, sell: 1 };
const ORDER = { market: 0, limit: 1, stop: 2 };
const TIF = { gtc: 0, ioc: 1 };
const UPDATES = ["accepted", "rejected", "cancelled", "filled"];
const REJECT_REASONS = ["unknown instrument", "invalid quantity", "invalid price", "insufficient cash", "insufficient position"];
const CANCEL_REASONS = ["requested", "immediate or cancel", "insufficient cash", "insufficient position", "end of data"];
const REFUSED = {
  "-1": "orders can only be sent from inside a callback",
  "-2": "that order has an invalid quantity, price or id",
  "-3": "a callback can send at most 1000 orders and cancels",
};

const HOOKS = ["setup", "onBar", "onFill", "onOrder", "onFinish"];

// Names the strategy's code sees in place of the page's own. There's no
// clock and no network, so a strategy can't learn what happens next by
// asking (a live price API would tell it the future of 2024); randomness
// is seeded, so every run of the same code gives the same result. This
// guards against accidents, not attacks: the worker is what contains code.
const SHADOWED = ["console", "Math", "Date", "performance", "fetch", "XMLHttpRequest", "WebSocket", "EventSource", "importScripts", "setTimeout", "setInterval"];
// Lines Function() adds before the code, plus the "use strict" line.
const LINE_OFFSET = 3;

export class StrategyError extends Error {
  constructor(message, line) {
    super(line ? `${message} (line ${line})` : message);
    this.name = "StrategyError";
    this.line = line;
  }
}

// The line in the strategy's code an error came from, if the browser says.
function errorLine(err) {
  const match = /(?:<anonymous>|> Function):(\d+):\d+/.exec(err?.stack ?? "");
  return match ? Number(match[1]) - LINE_OFFSET : undefined;
}

function describe(err) {
  const text = err instanceof Error ? `${err.name}: ${err.message}` : `threw ${String(err)}`;
  const line = errorLine(err);
  return line > 0 ? `${text} (line ${line})` : text;
}

function seededRandom(seed) {
  // mulberry32
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let t = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function noClock() {
  throw new Error("strategies have no clock; use bar.time or ctx.time");
}

class StrategyDate extends Date {
  constructor(...args) {
    if (args.length === 0) noClock();
    super(...args);
  }
  static now() {
    noClock();
  }
}

function formatArg(value) {
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value) ?? String(value);
  } catch {
    return String(value);
  }
}

// Compiles a strategy's source. Returns a function that makes a fresh
// instance, `(ctx, log) => hooks`. Throws a StrategyError for code that
// doesn't compile or doesn't define onBar.
export function compileStrategy(code) {
  if (typeof code !== "string" || !code.trim()) throw new StrategyError("the strategy has no code");
  let factory;
  try {
    factory = new Function(
      ...SHADOWED,
      `"use strict";\n${code}\n;return { ${HOOKS.map((h) => `${h}: typeof ${h} === "function" ? ${h} : undefined`).join(", ")} };`,
    );
  } catch (err) {
    throw new StrategyError(`${err.name}: ${err.message}`, errorLine(err));
  }
  const make = (log) => {
    const random = seededRandom(0x7a9e);
    const math = Object.create(Math, { random: { value: random } });
    const console = { log, info: log, warn: log, error: log, debug: log };
    return factory(console, math, StrategyDate, undefined, undefined, undefined, undefined, undefined, undefined, undefined, undefined);
  };
  let hooks;
  try {
    hooks = make(() => {});
  } catch (err) {
    throw new StrategyError(`${err.name}: ${err.message}`, errorLine(err));
  }
  if (!hooks.onBar) throw new StrategyError("define a function onBar(bar, ctx); it's called after every one-minute bar");
  return make;
}

export async function createEngine({ base = "./", fetch: fetchImpl = globalThis.fetch, symbol = "btcusdt" } = {}) {
  const url = (path) => new URL(path, new URL(base, globalThis.location?.href ?? "file:///")).toString();
  const load = async (path) => {
    const response = await fetchImpl(url(path));
    if (!response.ok) throw new Error(`couldn't load ${path}: HTTP ${response.status}`);
    return new Uint8Array(await response.arrayBuffer());
  };

  // The custom strategy being run, if any, and the current instance of it.
  let make = null;
  let hooks = null;
  let x = null; // the module's exports

  // Copies bytes into the module's input buffer. Reads memory.buffer after
  // every call, since the module's memory can grow and move.
  const write = (bytes) => {
    const ptr = x.tape_input(bytes.length);
    new Uint8Array(x.memory.buffer, ptr, bytes.length).set(bytes);
    return bytes.length;
  };
  const readOutput = (len) => decoder.decode(new Uint8Array(x.memory.buffer, x.tape_output(), len).slice());

  const log = (...args) => {
    x.tape_log(write(encoder.encode(args.map(formatArg).join(" "))));
  };

  function order(side, qty, options = {}) {
    if (typeof qty !== "number" || !(qty > 0)) {
      throw new TypeError(`an order needs a positive quantity of bitcoin, got ${formatArg(qty)}`);
    }
    const { limit, stop, ioc } = options;
    if (limit !== undefined && stop !== undefined) throw new TypeError("an order can have a limit price or a stop price, not both");
    const kind = limit !== undefined ? ORDER.limit : stop !== undefined ? ORDER.stop : ORDER.market;
    const price = limit ?? stop ?? 0;
    if (kind !== ORDER.market && !(typeof price === "number" && price > 0)) {
      throw new TypeError(`a ${kind === ORDER.limit ? "limit" : "stop"} price must be a positive number, got ${formatArg(price)}`);
    }
    // Market orders take what's there and cancel the rest, as the built-in
    // strategies' do; limit and stop orders wait until cancelled.
    const tif = (ioc ?? kind === ORDER.market) ? TIF.ioc : TIF.gtc;
    const id = x.tape_submit(side, kind, qty, price, tif);
    if (id < 0) throw new Error(REFUSED[id]);
    return id;
  }

  // What a strategy can read and do. Values are dollars and bitcoin.
  const ctx = Object.freeze({
    /** Close time of the bar being handled, in ms since 1970. */
    get time() { return x.tape_now(); },
    get cash() { return x.tape_cash(); },
    /** Bitcoin held. */
    get position() { return x.tape_position(); },
    /** Cash plus the position at the last price. */
    get equity() { return x.tape_equity(); },
    /** The last bar's close. */
    get price() { return x.tape_price(); },
    /** Orders sent and not yet finished, including ones on their way. */
    get openOrders() { return x.tape_open_orders(); },
    /** Buys `qty` bitcoin: at market, or pass { limit } or { stop }. Returns the order id. */
    buy: (qty, options) => order(SIDE.buy, qty, options),
    sell: (qty, options) => order(SIDE.sell, qty, options),
    cancel(id) {
      const result = x.tape_cancel(id);
      if (result < 0) throw new Error(REFUSED[result]);
    },
    /** The most bitcoin all the cash buys at `price` (default: the last price), after fees. */
    maxBuy: (price) => x.tape_max_buy(price ?? NaN),
    log,
  });

  // Each callback returns 0, or the length of an error message it wrote to
  // the module's input buffer.
  const call = (hook, ...args) => {
    try {
      hook?.(...args, ctx);
      return 0;
    } catch (err) {
      return write(encoder.encode(describe(err) || "threw an error"));
    }
  };
  const imports = {
    strategy: {
      on_start() {
        try {
          hooks = make(log);
        } catch (err) {
          return write(encoder.encode(describe(err)));
        }
        return call(hooks.setup);
      },
      on_bar: (time, open, high, low, close, volume) =>
        call(hooks.onBar, { time, open, high, low, close, volume }),
      on_fill: (id, time, side, price, qty, fee, liquidity) =>
        call(hooks.onFill, { id, time, side: side === SIDE.buy ? "buy" : "sell", price, qty, fee, maker: liquidity === 0 }),
      on_order(id, time, update, reason, remaining) {
        const status = UPDATES[update];
        const reasons = status === "rejected" ? REJECT_REASONS : status === "cancelled" ? CANCEL_REASONS : [];
        return call(hooks.onOrder, { id, time, status, reason: reasons[reason], remaining });
      },
      on_finish: () => call(hooks.onFinish),
    },
  };

  const { instance } = await WebAssembly.instantiate(await load("tape.wasm"), imports);
  x = instance.exports;

  const months = new Map();
  let loaded = null;

  async function loadMonth(month) {
    if (!/^\d{4}-\d{2}$/.test(month)) throw new Error(`month should look like 2024-01, got ${month}`);
    if (loaded === month) return;
    if (!months.has(month)) months.set(month, load(`data/${symbol}-${month}.bin`));
    const bytes = await months.get(month);
    const count = x.tape_load(write(bytes));
    if (count < 0) {
      // A negative count is minus the length of a JSON error.
      const { error } = JSON.parse(readOutput(-count));
      throw new Error(`couldn't read the data for ${month}: ${error}`);
    }
    loaded = month;
  }

  async function run({ code, ...params }) {
    make = params.strategy === "custom" ? compileStrategy(code) : null;
    await loadMonth(params.month);
    const len = write(encoder.encode(JSON.stringify(params)));
    const started = performance.now();
    let out;
    try {
      out = readOutput(x.tape_run(len));
    } finally {
      make = hooks = null;
    }
    const elapsedMs = performance.now() - started;
    const result = JSON.parse(out);
    if (result.error) {
      // A strategy that threw aborts the backtest; say so in its terms.
      const aborted = /^strategy aborted at (\S+): (.*) \(in (\w+)\)$/s.exec(result.error);
      if (aborted && params.strategy === "custom") {
        const line = /\(line (\d+)\)$/.exec(aborted[2]);
        const error = new StrategyError(`${aborted[2]}, in ${aborted[3]} at ${aborted[1]}`);
        error.line = line ? Number(line[1]) : undefined;
        throw error;
      }
      throw new Error(result.error);
    }
    return { ...result, elapsedMs };
  }

  return { run, preload: loadMonth };
}

// The same engine in a worker, so that a slow or stuck strategy can't
// freeze the page: a run that takes longer than `timeoutMs` (after its
// month has loaded) is stopped, and the next run starts a fresh worker.
//
//   const engine = createWorkerEngine({ base: "./engine/" });
//   const result = await engine.run(params);
export function createWorkerEngine({ base = "./", timeoutMs = 10_000 } = {}) {
  const workerUrl = new URL("./tape-worker.js", import.meta.url);
  const baseUrl = new URL(base, globalThis.location?.href).href;
  const pending = new Map();
  let worker = null;
  let nextId = 0;

  const settle = (id, settleWith) => {
    const job = pending.get(id);
    if (!job) return;
    clearTimeout(job.timer);
    pending.delete(id);
    settleWith(job);
  };
  const stopAll = (error) => {
    worker?.terminate();
    worker = null;
    for (const id of [...pending.keys()]) settle(id, (job) => job.reject(error));
  };

  function start() {
    const w = new Worker(workerUrl, { type: "module" });
    w.onmessage = ({ data }) => {
      if (data.running) {
        const job = pending.get(data.id);
        if (job) {
          job.timer = setTimeout(() => {
            stopAll(new StrategyError(`the backtest was stopped after ${timeoutMs / 1000} seconds; check for a loop that never ends`));
          }, timeoutMs);
        }
      } else if (data.error) {
        const { message, name, line } = data.error;
        settle(data.id, (job) => job.reject(name === "StrategyError" ? Object.assign(new StrategyError(message), { line }) : new Error(message)));
      } else {
        settle(data.id, (job) => job.resolve(data.result));
      }
    };
    w.onerror = (event) => {
      event.preventDefault?.();
      stopAll(new Error(`the engine worker failed: ${event.message ?? "unknown error"}`));
    };
    return w;
  }

  function run(params) {
    worker ??= start();
    const id = ++nextId;
    return new Promise((resolve, reject) => {
      pending.set(id, { resolve, reject, timer: null });
      worker.postMessage({ id, base: baseUrl, params });
    });
  }

  return { run, stop: () => stopAll(new Error("stopped")) };
}
