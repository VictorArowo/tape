// tape's backtesting engine, compiled to WebAssembly, for the browser.
//
//   import { createEngine } from "./engine/tape-engine.js";
//   const engine = await createEngine({ base: "./engine/" });
//   const result = await engine.run({ strategy: "sma", fast: 20, slow: 100,
//     month: "2024-01", execution: { latencyMs: 100, feeBps: 10,
//     slippageBps: 2, participationPct: 10, fillOnTouch: false } });
//
// `base` is where tape.wasm and data/ live. Months are fetched on first use
// and cached. Every run happens synchronously inside the module, so there's
// no worker; a month takes tens of milliseconds.

const decoder = new TextDecoder();
const encoder = new TextEncoder();

export async function createEngine({ base = "./", fetch: fetchImpl = globalThis.fetch, symbol = "btcusdt" } = {}) {
  const url = (path) => new URL(path, new URL(base, globalThis.location?.href ?? "file:///")).toString();
  const load = async (path) => {
    const response = await fetchImpl(url(path));
    if (!response.ok) throw new Error(`couldn't load ${path}: HTTP ${response.status}`);
    return new Uint8Array(await response.arrayBuffer());
  };

  const { instance } = await WebAssembly.instantiate(await load("tape.wasm"), {});
  const { memory, tape_input, tape_load, tape_run, tape_output } = instance.exports;

  // Copies bytes into the module's input buffer. Reads memory.buffer after
  // every call, since the module's memory can grow and move.
  const write = (bytes) => {
    const ptr = tape_input(bytes.length);
    new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
    return bytes.length;
  };
  const readOutput = (len) => decoder.decode(new Uint8Array(memory.buffer, tape_output(), len).slice());

  const months = new Map();
  let loaded = null;

  async function loadMonth(month) {
    if (!/^\d{4}-\d{2}$/.test(month)) throw new Error(`month should look like 2024-01, got ${month}`);
    if (loaded === month) return;
    if (!months.has(month)) months.set(month, load(`data/${symbol}-${month}.bin`));
    const bytes = await months.get(month);
    const count = tape_load(write(bytes));
    if (count < 0) {
      // A negative count is minus the length of a JSON error.
      const { error } = JSON.parse(readOutput(-count));
      throw new Error(`couldn't read the data for ${month}: ${error}`);
    }
    loaded = month;
  }

  async function run(params) {
    await loadMonth(params.month);
    const len = write(encoder.encode(JSON.stringify(params)));
    const started = performance.now();
    const out = readOutput(tape_run(len));
    const elapsedMs = performance.now() - started;
    const result = JSON.parse(out);
    if (result.error) throw new Error(result.error);
    return { ...result, elapsedMs };
  }

  return { run, preload: loadMonth };
}
