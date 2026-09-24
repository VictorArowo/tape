//! The backtest the web page asks for, and the answer it expects.
//!
//! Each run backtests the chosen strategy twice on the loaded month: once
//! with the execution settings from the page, and once with naive
//! execution, so the page can show both. Equity is sampled hourly.
//!
//! `"strategy": "custom"` runs the page's own JavaScript (see
//! [`crate::custom`]); the result then also carries what it logged.

use std::convert::Infallible;

use serde::Deserialize;
use serde_json::{Value, json};
use tape_core::{MarketEvent, Money, Side, TimeDelta};
use tape_engine::{Backtest, BacktestConfig, BacktestResult, ExecutionModel, FeeModel, Strategy};
use tape_strategies::{Breakout, BuyAndHold, MeanReversion, SmaCross};

use crate::format::Dataset;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Execution {
    pub latency_ms: f64,
    pub fee_bps: f64,
    pub slippage_bps: f64,
    pub participation_pct: f64,
    pub fill_on_touch: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Params {
    pub strategy: String,
    #[serde(default)]
    pub fast: f64,
    #[serde(default)]
    pub slow: f64,
    #[serde(default)]
    pub window: f64,
    #[serde(default)]
    pub band: f64,
    #[serde(default)]
    pub lookback: f64,
    pub execution: Execution,
    #[serde(default = "default_cash")]
    pub cash: f64,
}

fn default_cash() -> f64 {
    10_000.0
}

fn whole(value: f64, name: &str, min: f64, max: f64) -> Result<usize, String> {
    if !value.is_finite() || value.fract() != 0.0 || value < min || value > max {
        return Err(format!("{name} must be a whole number from {min} to {max}"));
    }
    Ok(value as usize)
}

fn in_range(value: f64, name: &str, min: f64, max: f64) -> Result<f64, String> {
    if !value.is_finite() || value < min || value > max {
        return Err(format!("{name} must be between {min} and {max}"));
    }
    Ok(value)
}

/// Makes the strategy for a `"custom"` run, fresh for each backtest.
pub type Custom<'a> = &'a dyn Fn() -> Result<Box<dyn Strategy>, String>;

/// Fills listed in a result; a busy strategy's later fills are left out.
const MAX_FILLS: usize = 5_000;

fn strategy(
    params: &Params,
    dataset: &Dataset,
    custom: Custom<'_>,
) -> Result<Box<dyn Strategy>, String> {
    let id = dataset.instrument.id;
    Ok(match params.strategy.as_str() {
        "sma" => {
            let fast = whole(params.fast, "fast average", 2.0, 5_000.0)?;
            let slow = whole(params.slow, "slow average", 3.0, 10_000.0)?;
            if fast >= slow {
                return Err("the fast average must be shorter than the slow one".into());
            }
            Box::new(SmaCross::new(id, fast, slow))
        }
        "meanrev" => Box::new(MeanReversion::new(
            id,
            whole(params.window, "window", 5.0, 10_000.0)?,
            in_range(params.band, "band", 0.1, 10.0)?,
        )),
        "breakout" => Box::new(Breakout::new(
            id,
            whole(params.lookback, "lookback", 2.0, 20_000.0)?,
        )),
        "hold" => Box::new(BuyAndHold::new(id)),
        "custom" => custom()?,
        other => return Err(format!("unknown strategy {other:?}")),
    })
}

/// The page's sliders, as an execution model. Basis points become parts
/// per million (1 bp = 100 ppm).
fn execution(e: &Execution) -> Result<ExecutionModel, String> {
    let latency = in_range(e.latency_ms, "latency", 0.0, 60_000.0)?;
    let fee = in_range(e.fee_bps, "fees", 0.0, 1_000.0)?;
    let slippage = in_range(e.slippage_bps, "slippage", 0.0, 1_000.0)?;
    let participation = in_range(e.participation_pct, "volume share", 0.01, 100.0)?;
    let ppm = |bps: f64| (bps * 100.0).round() as i64;
    Ok(ExecutionModel {
        latency: TimeDelta::from_micros((latency * 1_000.0).round() as i64),
        fees: FeeModel {
            maker_ppm: ppm(fee),
            taker_ppm: ppm(fee),
        },
        slippage_ppm: ppm(slippage),
        max_bar_participation_ppm: Some((participation * 10_000.0).round() as i64),
        require_trade_through: !e.fill_on_touch,
    })
}

fn backtest(
    dataset: &Dataset,
    params: &Params,
    model: ExecutionModel,
    custom: Custom<'_>,
) -> Result<BacktestResult, String> {
    let cash = Money::from_raw((in_range(params.cash, "cash", 1.0, 1e9)? * 1e8).round() as i64);
    let config = BacktestConfig {
        initial_cash: cash,
        execution: model,
        allow_short: false,
        equity_interval: TimeDelta::from_hours(1),
    };
    let mut strategy = strategy(params, dataset, custom)?;
    Backtest::new(config, [dataset.instrument.clone()])
        .run(
            strategy.as_mut(),
            dataset
                .bars
                .iter()
                .cloned()
                .map(|bar| Ok::<_, Infallible>(MarketEvent::Bar(bar))),
        )
        .map_err(|err| err.to_string())
}

fn max_drawdown(values: &[f64]) -> f64 {
    let mut peak = f64::MIN;
    let mut worst = 0.0_f64;
    for v in values {
        peak = peak.max(*v);
        if peak > 0.0 {
            worst = worst.min(v / peak - 1.0);
        }
    }
    worst
}

/// Runs a backtest and returns the result the page renders.
pub fn run(dataset: &Dataset, params: &Params) -> Result<Value, String> {
    run_with(dataset, params, &crate::custom_strategy)
}

/// [`run`], with `custom` making the strategy for `"custom"` runs.
pub fn run_with(dataset: &Dataset, params: &Params, custom: Custom<'_>) -> Result<Value, String> {
    let chosen = backtest(dataset, params, execution(&params.execution)?, custom)?;
    let log = match params.strategy.as_str() {
        "custom" => crate::custom::take_log(),
        _ => Vec::new(),
    };
    let naive = backtest(dataset, params, ExecutionModel::naive(), custom)?;

    // Both runs see the same bars, so they sample equity at the same times.
    let dates: Vec<String> = chosen.equity.iter().map(|p| p.ts.to_string()).collect();
    let equity: Vec<f64> = chosen.equity.iter().map(|p| p.equity.to_f64()).collect();
    let naive_equity: Vec<f64> = naive
        .equity
        .iter()
        .map(|p| p.equity.to_f64())
        .take(equity.len())
        .collect();
    let initial = params.cash;
    let total = |r: &BacktestResult| r.final_equity.to_f64() / initial - 1.0;

    Ok(json!({
        "placeholder": false,
        "engine": concat!("tape ", env!("CARGO_PKG_VERSION")),
        "strategy": chosen.strategy,
        "symbol": dataset.instrument.symbol,
        "events": chosen.events,
        "runHash": chosen.run_hash,
        "naiveRunHash": naive.run_hash,
        "dates": dates,
        "equity": equity,
        "naiveEquity": naive_equity,
        // [time in ms, "buy" or "sell", price, quantity]
        "fills": chosen.fills.iter().take(MAX_FILLS).map(|f| json!([
            f.ts.as_nanos() / 1_000_000,
            if f.side == Side::Buy { "buy" } else { "sell" },
            f.price.to_f64(),
            f.qty.to_f64(),
        ])).collect::<Vec<_>>(),
        "log": log,
        "stats": {
            "totalReturn": total(&chosen),
            "naiveReturn": total(&naive),
            "maxDrawdown": max_drawdown(&equity),
            "naiveMaxDrawdown": max_drawdown(&naive_equity),
            "trades": chosen.fills.len(),
            "naiveTrades": naive.fills.len(),
            "feesPaid": chosen.fees_paid.to_f64(),
            "finalEquity": chosen.final_equity.to_f64(),
            "naiveFinalEquity": naive.final_equity.to_f64(),
        },
    }))
}

/// Parses the page's parameters and runs, returning JSON either way.
pub fn run_json(dataset: Option<&Dataset>, params: &str) -> String {
    let result = (|| {
        let dataset = dataset.ok_or("no month is loaded")?;
        let params: Params =
            serde_json::from_str(params).map_err(|err| format!("bad parameters: {err}"))?;
        run(dataset, &params)
    })();
    match result {
        Ok(value) => value.to_string(),
        Err(message) => json!({ "error": message }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom;
    use crate::format::{decode, encode};
    use tape_core::{Bar, Fill, Instrument, InstrumentId, Price, Qty, Timestamp, abi};
    use tape_engine::OrderUpdate;

    fn dataset() -> Dataset {
        let instrument = Instrument::new(
            InstrumentId(1),
            "BTCUSDT",
            "0.01".parse().unwrap(),
            "0.00001".parse().unwrap(),
        );
        let start = Timestamp::from_ymd(2024, 1, 1);
        let mut price = 4_200_000_000_000_i64;
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let bars: Vec<Bar> = (0..3_000)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let open = price;
                let close = open + ((seed % 4_001) as i64 - 2_000) * 1_000_000;
                price = close;
                let open_ts = start + TimeDelta::from_mins(i);
                Bar {
                    open_ts,
                    close_ts: open_ts + TimeDelta::from_mins(1),
                    instrument: InstrumentId(1),
                    open: Price::from_raw(open),
                    high: Price::from_raw(open.max(close) + 2_000_000_000),
                    low: Price::from_raw(open.min(close) - 2_000_000_000),
                    close: Price::from_raw(close),
                    volume: Qty::from_int(50).unwrap(),
                }
            })
            .collect();
        decode(&encode(&instrument, &bars).unwrap()).unwrap()
    }

    fn params(strategy: &str) -> String {
        json!({
            "strategy": strategy, "fast": 20, "slow": 100, "window": 60, "band": 2, "lookback": 240,
            "month": "2024-01",
            "execution": {"latencyMs": 100, "feeBps": 10, "slippageBps": 2, "participationPct": 10, "fillOnTouch": false}
        })
        .to_string()
    }

    #[test]
    fn runs_every_strategy_in_the_pages_shape() {
        let data = dataset();
        for strategy in ["sma", "meanrev", "breakout", "hold"] {
            let out: Value =
                serde_json::from_str(&run_json(Some(&data), &params(strategy))).unwrap();
            assert!(out.get("error").is_none(), "{strategy}: {out}");
            let n = out["dates"].as_array().unwrap().len();
            assert!(n >= 48, "{strategy}: hourly samples over two days");
            assert_eq!(out["equity"].as_array().unwrap().len(), n);
            assert_eq!(out["naiveEquity"].as_array().unwrap().len(), n);
            assert_eq!(out["events"], 3_000);
            assert!(out["stats"]["maxDrawdown"].as_f64().unwrap() <= 0.0);
        }
    }

    #[test]
    fn matches_the_engine_run_directly() {
        let data = dataset();
        let out: Value = serde_json::from_str(&run_json(Some(&data), &params("sma"))).unwrap();
        let config = BacktestConfig {
            initial_cash: Money::from_int(10_000).unwrap(),
            execution: ExecutionModel::realistic(),
            allow_short: false,
            equity_interval: TimeDelta::from_hours(1),
        };
        let direct = Backtest::new(config, [data.instrument.clone()])
            .run(
                &mut SmaCross::new(InstrumentId(1), 20, 100),
                data.bars
                    .iter()
                    .cloned()
                    .map(|b| Ok::<_, Infallible>(MarketEvent::Bar(b))),
            )
            .unwrap();
        // The page's "realistic" sliders are exactly ExecutionModel::realistic().
        assert_eq!(out["runHash"], direct.run_hash);
    }

    #[test]
    fn explains_bad_input() {
        let data = dataset();
        let bad = params("sma").replace("\"slow\":100", "\"slow\":10");
        let out: Value = serde_json::from_str(&run_json(Some(&data), &bad)).unwrap();
        assert!(out["error"].as_str().unwrap().contains("shorter"));
        let out: Value = serde_json::from_str(&run_json(None, &params("sma"))).unwrap();
        assert_eq!(out["error"], "no month is loaded");
        let out: Value = serde_json::from_str(&run_json(Some(&data), "{nope")).unwrap();
        assert!(out["error"].as_str().unwrap().starts_with("bad parameters"));
    }

    /// The reference crossover, rewritten against the bridge a page's
    /// JavaScript uses: sums of closes in cents, sizing with `max_buy`.
    struct BridgeCross {
        closes: Vec<f64>,
        fast: usize,
        slow: usize,
    }

    impl custom::Callbacks for BridgeCross {
        fn on_start(&mut self) -> Result<(), String> {
            self.closes.clear();
            Ok(())
        }

        fn on_bar(&mut self, bar: &Bar) -> Result<(), String> {
            self.closes.push((bar.close.to_f64() * 100.0).round());
            let n = self.closes.len();
            if n < self.slow || custom::open_orders() > 0 {
                return Ok(());
            }
            let sum = |len: usize| self.closes[n - len..].iter().sum::<f64>();
            let want_long = sum(self.fast) * self.slow as f64 > sum(self.slow) * self.fast as f64;
            let position = custom::position();
            if want_long && position == 0.0 {
                let qty = custom::max_buy(bar.close.to_f64());
                if qty > 0.0 {
                    custom::submit(abi::SIDE_BUY, abi::ORDER_MARKET, qty, 0.0, abi::TIF_IOC);
                }
            } else if !want_long && position > 0.0 {
                custom::submit(
                    abi::SIDE_SELL,
                    abi::ORDER_MARKET,
                    position,
                    0.0,
                    abi::TIF_IOC,
                );
            }
            Ok(())
        }

        fn on_fill(&mut self, _: &Fill) -> Result<(), String> {
            Ok(())
        }

        fn on_order(&mut self, _: &OrderUpdate) -> Result<(), String> {
            Ok(())
        }

        fn on_finish(&mut self) -> Result<(), String> {
            custom::log(&format!("saw {} bars", self.closes.len()));
            Ok(())
        }
    }

    fn run_custom<C: custom::Callbacks + 'static>(make: impl Fn() -> C) -> Value {
        let data = dataset();
        let params: Params = serde_json::from_str(&params("custom")).unwrap();
        run_with(&data, &params, &|| {
            Ok(Box::new(custom::JsStrategy::new(make())) as Box<dyn Strategy>)
        })
        .unwrap_or_else(|err| json!({ "error": err }))
    }

    #[test]
    fn a_custom_strategy_gets_the_same_engine() {
        let custom = run_custom(|| BridgeCross {
            closes: Vec::new(),
            fast: 20,
            slow: 100,
        });
        let native: Value =
            serde_json::from_str(&run_json(Some(&dataset()), &params("sma"))).unwrap();
        assert!(custom["stats"]["trades"].as_u64().unwrap() > 10);
        assert_eq!(custom["runHash"], native["runHash"]);
        assert_eq!(custom["naiveRunHash"], native["naiveRunHash"]);
        assert_eq!(custom["stats"], native["stats"]);
        assert_eq!(custom["strategy"], "custom");
        // Only the chosen run's log comes back.
        assert_eq!(custom["log"], json!(["saw 3000 bars"]));
        assert_eq!(native["log"], json!([]));
        assert_eq!(
            custom["fills"].as_array().unwrap().len(),
            native["fills"].as_array().unwrap().len()
        );
    }

    /// Buys an awkward amount at an awkward limit price on the first bar,
    /// then records what the engine made of it.
    struct Awkward;

    impl custom::Callbacks for Awkward {
        fn on_start(&mut self) -> Result<(), String> {
            assert_eq!(custom::cash(), 10_000.0);
            assert!(custom::price().is_nan());
            // Nothing to buy with before a price is known.
            assert_eq!(custom::max_buy(f64::NAN), 0.0);
            Ok(())
        }

        fn on_bar(&mut self, bar: &Bar) -> Result<(), String> {
            if custom::now() == bar.close_ts.as_nanos() as f64 / 1e6
                && custom::position() == 0.0
                && custom::open_orders() == 0
            {
                let limit = custom::price() * 1.001 + 0.004_9;
                let id = custom::submit(
                    abi::SIDE_BUY,
                    abi::ORDER_LIMIT,
                    0.012_345_67,
                    limit,
                    abi::TIF_GTC,
                );
                assert_eq!(id, 1.0);
                assert_eq!(custom::cancel(0.5), custom::BAD_ARGUMENTS);
                assert_eq!(
                    custom::submit(7, abi::ORDER_MARKET, 1.0, 0.0, abi::TIF_GTC),
                    custom::BAD_ARGUMENTS
                );
                assert_eq!(
                    custom::submit(
                        abi::SIDE_BUY,
                        abi::ORDER_MARKET,
                        f64::INFINITY,
                        0.0,
                        abi::TIF_GTC
                    ),
                    custom::BAD_ARGUMENTS
                );
            }
            Ok(())
        }

        fn on_fill(&mut self, fill: &Fill) -> Result<(), String> {
            custom::log(&format!("{} {}", fill.qty, fill.price));
            Err("stop here".into())
        }

        fn on_order(&mut self, _: &OrderUpdate) -> Result<(), String> {
            Ok(())
        }

        fn on_finish(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn rounds_orders_onto_the_lot_and_tick_and_reports_errors() {
        let out = run_custom(|| Awkward);
        let error = out["error"].as_str().unwrap();
        assert!(error.contains("stop here (in onFill)"), "{error}");
        assert_eq!(custom::take_log().len(), 1);
        // Outside a callback there's nothing to read or do.
        assert!(custom::cash().is_nan());
        assert_eq!(
            custom::submit(abi::SIDE_BUY, abi::ORDER_MARKET, 1.0, 0.0, abi::TIF_GTC),
            custom::NOT_IN_CALLBACK
        );
    }

    #[test]
    fn logs_the_quantity_after_rounding_down_to_whole_lots() {
        struct Once(bool);
        impl custom::Callbacks for Once {
            fn on_start(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn on_bar(&mut self, _: &Bar) -> Result<(), String> {
                if !self.0 {
                    self.0 = true;
                    custom::submit(
                        abi::SIDE_BUY,
                        abi::ORDER_MARKET,
                        0.012_345_67,
                        0.0,
                        abi::TIF_GTC,
                    );
                }
                Ok(())
            }
            fn on_fill(&mut self, fill: &Fill) -> Result<(), String> {
                custom::log(&fill.qty.to_string());
                Ok(())
            }
            fn on_order(&mut self, _: &OrderUpdate) -> Result<(), String> {
                Ok(())
            }
            fn on_finish(&mut self) -> Result<(), String> {
                Ok(())
            }
        }
        let out = run_custom(|| Once(false));
        assert_eq!(out["log"], json!(["0.01234"]));
    }

    #[test]
    fn custom_runs_need_the_browser() {
        let out: Value =
            serde_json::from_str(&run_json(Some(&dataset()), &params("custom"))).unwrap();
        assert!(out["error"].as_str().unwrap().contains("browser"));
    }
}
