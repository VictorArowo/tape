# tape site

Static marketing site for tape. No build step: serve this folder as-is.

```sh
python3 -m http.server 8770 --directory site
```

## Result data

The results sections read JSON from `data/results/`:

- `honest-vs-naive.json`: the same strategy run with naive and realistic execution
- `experiment.json`: the AI research experiment

Both are currently **placeholder data** (`"placeholder": true`), and each section shows a
"Placeholder data" flag until the file is replaced with real output from the engine.
The demo in `demo.js` runs a placeholder engine (`ENGINE_IS_PLACEHOLDER`) until the
WebAssembly build is wired into `runBacktest`.

Do not launch the site while any placeholder flag is visible.

`data/btcusdt-2024-01-03.json` is real: one-minute closes from Binance's public archive.
