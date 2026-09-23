//! `tape compare`: how much of a backtest's result survives realistic
//! execution, one assumption at a time.
//!
//! The same moving-average crossover is run as a ladder of increasingly
//! honest backtests:
//!
//! 1. a notebook-style vectorized backtest with a one-bar look-ahead bug,
//! 2. the engine with naive execution (next-bar fills, no costs),
//! 3. plus exchange fees,
//! 4. plus slippage,
//! 5. plus latency,
//! 6. plus volume limits and trade-through for resting orders, which is
//!    the full realistic model.
//!
//! With `--json`, writes the result in the format the project site reads.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, ensure};
use clap::Args;
use serde_json::json;
use tape_core::{InstrumentId, MarketEvent, Money, Price, TimeDelta, Timestamp};
use tape_data::binance::Period;
use tape_engine::{Backtest, BacktestConfig, BacktestResult, ExecutionModel, FeeModel};
use tape_metrics::{Metrics, Periods};
use tape_strategies::{BuyAndHold, SmaCross, notebook};

use crate::run::{instrument, load, money, pct, ratio};

#[derive(Args)]
pub struct CompareArgs {
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    /// First month, as YYYY-MM.
    #[arg(long)]
    pub from: String,
    /// Last month (inclusive), as YYYY-MM. Defaults to --from.
    #[arg(long)]
    pub to: Option<String>,
    #[arg(long, default_value = "1m")]
    interval: String,
    #[arg(long, default_value_t = 60)]
    fast: usize,
    #[arg(long, default_value_t = 240)]
    slow: usize,
    #[arg(long, default_value = "10000")]
    cash: Money,
    /// Write the comparison as JSON for the project site.
    #[arg(long)]
    json: Option<PathBuf>,
}

struct Rung {
    key: &'static str,
    label: &'static str,
    /// Equity over time, as (timestamp, value).
    equity: Vec<(Timestamp, f64)>,
    metrics: Metrics,
    fills: usize,
    fees: f64,
    run_hash: Option<String>,
}

/// Each rung adds one realistic assumption to the one before.
fn ladder() -> Vec<(&'static str, &'static str, ExecutionModel)> {
    let naive = ExecutionModel::naive();
    let fees = ExecutionModel {
        fees: FeeModel::BINANCE_SPOT,
        ..naive.clone()
    };
    let slippage = ExecutionModel {
        slippage_ppm: ExecutionModel::realistic().slippage_ppm,
        ..fees.clone()
    };
    let latency = ExecutionModel {
        latency: ExecutionModel::realistic().latency,
        ..slippage.clone()
    };
    let realistic = ExecutionModel::realistic();
    let volume = ExecutionModel {
        max_bar_participation_ppm: realistic.max_bar_participation_ppm,
        require_trade_through: realistic.require_trade_through,
        ..latency.clone()
    };
    assert_eq!(
        volume, realistic,
        "the ladder must end at the realistic model"
    );
    vec![
        ("naive", "Engine, naive execution", naive),
        ("fees", "+ 0.1% exchange fees", fees),
        ("slippage", "+ 0.02% slippage", slippage),
        ("latency", "+ 100 ms latency", latency),
        ("tape", "+ volume limits (realistic)", volume),
    ]
}

fn to_points(result: &BacktestResult) -> Vec<(Timestamp, f64)> {
    result
        .equity
        .iter()
        .map(|p| (p.ts, p.equity.to_f64()))
        .collect()
}

fn metrics_of(points: &[(Timestamp, f64)]) -> Result<Metrics> {
    let equity: Vec<tape_engine::EquityPoint> = points
        .iter()
        .map(|(ts, v)| tape_engine::EquityPoint {
            ts: *ts,
            equity: Money::from_raw((v * 1e8).round() as i64),
        })
        .collect();
    tape_metrics::compute(&equity, Periods::DAILY_24_7).context("no equity samples")
}

/// Equity at the end of each UTC day from `first_day` for `days` days: the
/// last sample at or before the following midnight.
fn daily(points: &[(Timestamp, f64)], first_day: Timestamp, days: usize, initial: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(days);
    let mut i = 0;
    let mut value = initial;
    for day in 0..days {
        let end = first_day + TimeDelta::from_days(day as i64 + 1);
        while i < points.len() && points[i].0 <= end {
            value = points[i].1;
            i += 1;
        }
        out.push(value);
    }
    out
}

fn date(ts: Timestamp) -> String {
    let (y, m, d) = ts.date();
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn compare(data_dir: &Path, args: CompareArgs, periods: Vec<Period>) -> Result<()> {
    let instrument = instrument(&args.symbol)?;
    let id = InstrumentId(1);
    let initial = args.cash.to_f64();
    let mut rungs = Vec::new();

    let closes: Vec<(Timestamp, Price)> = load(data_dir, &args.symbol, &args.interval, &periods)?
        .filter_map(|event| match event {
            Ok(MarketEvent::Bar(bar)) => Some(Ok((bar.close_ts, bar.close))),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        })
        .collect::<Result<_, _>>()?;
    ensure!(!closes.is_empty(), "no bars in the selected period");
    let notebook = notebook::sma_cross_with_lookahead(&closes, args.fast, args.slow, initial);
    rungs.push(Rung {
        key: "notebook",
        label: "Notebook, one-bar look-ahead",
        metrics: metrics_of(&notebook)?,
        equity: notebook,
        fills: 0,
        fees: 0.0,
        run_hash: None,
    });

    let config = |execution| BacktestConfig {
        initial_cash: args.cash,
        execution,
        ..BacktestConfig::default()
    };
    for (key, label, execution) in ladder() {
        let backtest = Backtest::new(config(execution), [instrument.clone()]);
        let result = backtest
            .run(
                &mut SmaCross::new(id, args.fast, args.slow),
                load(data_dir, &args.symbol, &args.interval, &periods)?,
            )
            .with_context(|| format!("backtest failed at rung {key}"))?;
        let equity = to_points(&result);
        rungs.push(Rung {
            key,
            label,
            metrics: metrics_of(&equity)?,
            equity,
            fills: result.fills.len(),
            fees: result.fees_paid.to_f64(),
            run_hash: Some(result.run_hash),
        });
    }

    let benchmark = Backtest::new(config(ExecutionModel::realistic()), [instrument])
        .run(
            &mut BuyAndHold::new(id),
            load(data_dir, &args.symbol, &args.interval, &periods)?,
        )
        .context("buy-and-hold benchmark failed")?;
    let benchmark_equity = to_points(&benchmark);
    let benchmark_metrics = metrics_of(&benchmark_equity)?;

    let strategy = format!(
        "Moving-average crossover ({} and {} one-minute bars)",
        args.fast, args.slow
    );
    // Bars are stamped at their close, so the last one closes at midnight
    // after the final day; step back a nanosecond to name that day.
    let last_day = date(closes.last().unwrap().0 - TimeDelta::from_nanos(1));
    println!(
        "{strategy} on {}, {} → {last_day}",
        args.symbol,
        date(closes[0].0)
    );
    println!();
    println!(
        "{:<32}{:>14}{:>10}{:>10}{:>8}{:>12}",
        "", "final equity", "return", "sharpe", "fills", "fees"
    );
    for rung in &rungs {
        println!(
            "{:<32}{:>14}{:>10}{:>10}{:>8}{:>12}",
            rung.label,
            money(rung.metrics.final_equity),
            pct(rung.metrics.total_return),
            ratio(rung.metrics.sharpe),
            rung.fills,
            money(rung.fees)
        );
    }
    println!(
        "{:<32}{:>14}{:>10}{:>10}{:>8}{:>12}",
        "Buy and hold (realistic)",
        money(benchmark_metrics.final_equity),
        pct(benchmark_metrics.total_return),
        ratio(benchmark_metrics.sharpe),
        benchmark.fills.len(),
        money(benchmark.fees_paid.to_f64())
    );

    if let Some(path) = &args.json {
        // Days covered, from the first bar's day to the last bar's day.
        let first_day = Timestamp::from_ymd(
            closes[0].0.date().0,
            closes[0].0.date().1,
            closes[0].0.date().2,
        );
        let last_bar = closes.last().unwrap().0 - TimeDelta::from_nanos(1);
        let days =
            ((last_bar - first_day).as_nanos() / TimeDelta::from_days(1).as_nanos()) as usize + 1;
        let dates: Vec<String> = (0..days)
            .map(|d| date(first_day + TimeDelta::from_days(d as i64)))
            .collect();

        let stats = |m: &Metrics, fills: usize, fees: f64| {
            let round = |v: f64, places: i32| (v * 10f64.powi(places)).round() / 10f64.powi(places);
            json!({
                "totalReturn": round(m.total_return, 4),
                "sharpe": m.sharpe.map(|s| round(s, 2)),
                "maxDrawdown": round(-m.max_drawdown, 4),
                "trades": fills,
                "feesPaid": round(fees, 2),
                "finalEquity": round(m.final_equity, 2),
            })
        };
        let mut series = serde_json::Map::new();
        let mut all_stats = serde_json::Map::new();
        for rung in &rungs {
            let values: Vec<f64> = daily(&rung.equity, first_day, days, initial)
                .into_iter()
                .map(|v| (v * 100.0).round() / 100.0)
                .collect();
            series.insert(rung.key.into(), json!(values));
            all_stats.insert(rung.key.into(), stats(&rung.metrics, rung.fills, rung.fees));
        }
        let bh: Vec<f64> = daily(&benchmark_equity, first_day, days, initial)
            .into_iter()
            .map(|v| (v * 100.0).round() / 100.0)
            .collect();
        series.insert("buyHold".into(), json!(bh));
        all_stats.insert(
            "buyHold".into(),
            stats(
                &benchmark_metrics,
                benchmark.fills.len(),
                benchmark.fees_paid.to_f64(),
            ),
        );

        let ladder: Vec<_> = rungs
            .iter()
            .map(|r| {
                json!({
                    "key": r.key,
                    "label": r.label,
                    "stats": stats(&r.metrics, r.fills, r.fees),
                    "runHash": r.run_hash,
                })
            })
            .collect();
        let output = json!({
            "placeholder": false,
            "strategy": strategy,
            "instrument": args.symbol.replace("USDT", "/USDT"),
            "from": dates.first(),
            "to": dates.last(),
            "initialCash": initial,
            "dates": dates,
            "series": series,
            "stats": all_stats,
            "ladder": ladder,
            "generatedBy": format!(
                "tape compare --symbol {} --from {} --to {} --fast {} --slow {} --cash {}",
                args.symbol,
                args.from,
                args.to.as_deref().unwrap_or(&args.from),
                args.fast,
                args.slow,
                args.cash
            ),
        });
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_vec_pretty(&output)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
        eprintln!("\nwrote {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ladder_ends_at_the_realistic_model() {
        let rungs = ladder();
        assert_eq!(rungs.last().unwrap().2, ExecutionModel::realistic());
        assert_eq!(rungs[0].2, ExecutionModel::naive());
    }

    #[test]
    fn daily_takes_the_last_value_before_each_midnight() {
        let day0 = Timestamp::from_ymd(2024, 1, 1);
        let points = vec![
            (day0 + TimeDelta::from_hours(1), 1.0),
            (day0 + TimeDelta::from_days(1), 2.0), // the bar that closes at midnight
            (
                day0 + TimeDelta::from_days(1) + TimeDelta::from_mins(1),
                3.0,
            ),
        ];
        assert_eq!(daily(&points, day0, 3, 0.0), vec![2.0, 3.0, 3.0]);
    }
}
