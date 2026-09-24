// Runs backtests for createWorkerEngine() in tape-engine.js, off the page.
// It tells the page when a run's data has loaded and the backtest itself
// starts, so the page can time the backtest alone and stop it if it hangs.

import { createEngine } from "./tape-engine.js";

let engine = null;

self.onmessage = async ({ data: { id, base, params } }) => {
  try {
    engine ??= createEngine({ base }).catch((err) => {
      engine = null;
      throw err;
    });
    const ready = await engine;
    await ready.preload(params.month);
    self.postMessage({ id, running: true });
    self.postMessage({ id, result: await ready.run(params) });
  } catch (err) {
    self.postMessage({ id, error: { name: err?.name, message: err?.message ?? String(err), line: err?.line } });
  }
};
