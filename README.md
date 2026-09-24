# tape

A backtesting engine that doesn't lie, written in Rust.

Most backtests look better than reality because they quietly cheat: they peek at data that wasn't available yet, fill orders at prices nobody would have given them, and ignore fees, slippage and latency. tape is built so those mistakes are hard or impossible to make:

- **No look-ahead.** Strategies only ever see events up to the current simulated time. Bars become visible when they close, not when they open.
- **Deterministic.** The same data, strategy and settings produce bit-identical results, every run.
- **Exact accounting.** Prices, quantities and money are fixed-point integers, so cash and positions always reconcile. Floating point is only used for statistics.
- **Realistic execution.** Latency, maker/taker fees, slippage and partial fills are part of the model, not an afterthought.

On top of the engine sits an experiment: AI agents that propose, write and test trading strategies inside a sandbox, with the engine's rules and a held-out test period keeping them honest.

## Status

Early. Research and paper trading only.

| Milestone | |
|---|---|
| M0 Workspace, CI | done |
| M1 Core types, market data loading | done |
| M2 Engine, execution model, portfolio, metrics, CLI | done |
| M3 Honest vs naive backtest comparison | done |
| M4 WebAssembly strategy sandbox | done |
| M5 Agent research desk | done: 200 attempts, results/lab/full-1 |
| M6 Web demo and write-up | in-browser engine done; write-up pending |

## Layout

```
crates/
  tape-core        fixed-point numbers, timestamps, market events, orders
  tape-data        Binance public data, checksummed downloads, Parquet storage, time-ordered merging
  tape-engine      event loop, simulated exchange, portfolio accounting
  tape-metrics     returns, Sharpe, Sortino, drawdowns
  tape-strategies  reference strategies (buy and hold, moving-average crossover)
  tape-guest       SDK for strategies compiled to WebAssembly
  tape-wasm        the sandbox that runs them: capability, compute and memory limits
  tape-lab         the AI research desk: propose, write, backtest, critique, then one held-out test
  tape-web         the engine compiled to WebAssembly for the browser demo, including custom JavaScript strategies
  tape-cli         the `tape` command
web/
  tape-engine.js   loader for the browser build; build.sh writes web/dist, test.mjs checks it against native
  tape-worker.js   runs the engine off the page, so a runaway custom strategy can be stopped
  strategies.js    example custom strategies
guests/
  sma-cross        the reference crossover, compiled for the sandbox
```

## The sandbox

Strategies can be compiled to WebAssembly and run inside a sandbox, so code nobody has reviewed (for example, code an AI agent just wrote) can be backtested safely. A module can only import the functions tape provides: there's no clock, randomness, file system or network. Each callback gets a fixed compute budget, memory is capped, and floating point is made deterministic. The sandboxed crossover in `guests/sma-cross` produces bit-identical orders, fills and run hashes to the native one.

```sh
cargo build --release --target wasm32-unknown-unknown \
  --manifest-path guests/sma-cross/Cargo.toml --target-dir target/guests
cargo run --release -p tape-cli -- run wasm --from 2024-01 \
  --module target/guests/wasm32-unknown-unknown/release/tape_guest_sma_cross.wasm
```

## Custom strategies in the browser

The browser build can run a strategy written in JavaScript through the real engine. The code defines `onBar(bar, ctx)` (and optionally `setup`, `onFill`, `onOrder`, `onFinish`) and trades with `ctx.buy` and `ctx.sell`. It works like the sandbox: before each callback the engine takes a snapshot of what the strategy may see, and afterwards it replays the strategy's orders through the same execution model. So the strategy gets exactly what a native one would, and nothing later. There's no clock or network, randomness is seeded, and a run in the worker that doesn't finish is stopped. `web/test.mjs` checks that the moving-average crossover written in JavaScript produces the same run hashes as the native one.

## Try it

```sh
cargo run --release -p tape-cli -- data fetch BTCUSDT --from 2024-01
cargo run --release -p tape-cli -- run sma-cross --from 2024-01 --fast 30 --slow 120
```

The second command runs the same strategy twice, once with the naive assumptions most quick backtests make (instant, free, unlimited fills) and once with latency, fees, slippage and volume limits. On January 2024 BTC/USDT one-minute bars, that moving-average crossover makes +12.4% under the first and loses 35.2% under the second.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Licence

MIT
