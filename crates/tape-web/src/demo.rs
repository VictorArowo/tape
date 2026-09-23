//! The backtest the web page asks for, and the answer it expects.
//!
//! Each run backtests the chosen strategy twice on the loaded month: once
//! with the execution settings from the page, and once with naive
//! execution, so the page can show both. Equity is sampled hourly.

use std::convert::Infallible;

use serde::Deserialize;
use serde_json::{Value, json};
use tape_core::{MarketEvent, Money, TimeDelta};
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

fn strategy(params: &Params, dataset: &Dataset) -> Result<Box<dyn Strategy>, String> {
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
) -> Result<BacktestResult, String> {
    let cash = Money::from_raw((in_range(params.cash, "cash", 1.0, 1e9)? * 1e8).round() as i64);
    let config = BacktestConfig {
        initial_cash: cash,
        execution: model,
        allow_short: false,
        equity_interval: TimeDelta::from_hours(1),
    };
    let mut strategy = strategy(params, dataset)?;
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
    let chosen = backtest(dataset, params, execution(&params.execution)?)?;
    let naive = backtest(dataset, params, ExecutionModel::naive())?;

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
    use crate::format::{decode, encode};
    use tape_core::{Bar, Instrument, InstrumentId, Price, Qty, Timestamp};

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
}
