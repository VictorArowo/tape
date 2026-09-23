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
| M3 Honest vs naive backtest comparison | |
| M4 WebAssembly strategy sandbox | |
| M5 Agent research desk | |
| M6 Web demo and write-up | |

## Layout

```
crates/
  tape-core        fixed-point numbers, timestamps, market events, orders
  tape-data        Binance public data, checksummed downloads, Parquet storage, time-ordered merging
  tape-engine      event loop, simulated exchange, portfolio accounting
  tape-metrics     returns, Sharpe, Sortino, drawdowns
  tape-strategies  reference strategies (buy and hold, moving-average crossover)
  tape-cli         the `tape` command
```

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
