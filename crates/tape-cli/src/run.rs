//! `tape run`: backtest a reference strategy on stored data.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use clap::{Args, ValueEnum};
use tape_core::{Instrument, InstrumentId, MarketEvent, Money};
use tape_data::Fetcher;
use tape_data::binance::{ArchiveFile, Dataset, Period};
use tape_data::store;
use tape_engine::{Backtest, BacktestConfig, BacktestResult, ExecutionModel, Strategy};
use tape_metrics::{Metrics, Periods};
use tape_strategies::{BuyAndHold, SmaCross};

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum StrategyName {
    BuyAndHold,
    SmaCross,
    /// A strategy compiled to WebAssembly, run in the sandbox (needs --module).
    Wasm,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Model {
    Naive,
    Realistic,
    /// Run both and compare them side by side.
    Both,
}

#[derive(Args)]
pub struct RunArgs {
    strategy: StrategyName,
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    /// First month, as YYYY-MM.
    #[arg(long)]
    from: String,
    /// Last month (inclusive), as YYYY-MM. Defaults to --from.
    #[arg(long)]
    to: Option<String>,
    #[arg(long, default_value = "1m")]
    interval: String,
    #[arg(long, value_enum, default_value = "both")]
    model: Model,
    #[arg(long, default_value = "10000")]
    cash: Money,
    /// Fast moving average length, in bars (sma-cross).
    #[arg(long, default_value_t = 20)]
    fast: usize,
    /// Slow moving average length, in bars (sma-cross).
    #[arg(long, default_value_t = 60)]
    slow: usize,
    /// The compiled strategy to run with `wasm`.
    #[arg(long, required_if_eq("strategy", "wasm"))]
    module: Option<PathBuf>,
    /// Write full results (orders, fills, equity curve) as JSON.
    #[arg(long)]
    json: Option<PathBuf>,
}

/// Trading rules for symbols we know; others need to be added here.
pub(crate) fn instrument(symbol: &str) -> Result<Instrument> {
    let (tick, lot) = match symbol {
        "BTCUSDT" => ("0.01", "0.00001"),
        "ETHUSDT" => ("0.01", "0.0001"),
        "SOLUSDT" => ("0.01", "0.001"),
        other => bail!("no trading rules for {other}; add its tick and lot size in run.rs"),
    };
    Ok(Instrument::new(
        InstrumentId(1),
        symbol,
        tick.parse()?,
        lot.parse()?,
    ))
}

pub(crate) fn load(
    data_dir: &Path,
    symbol: &str,
    interval: &str,
    periods: &[Period],
) -> Result<impl Iterator<Item = Result<MarketEvent, tape_data::DataError>> + use<>> {
    let fetcher = Fetcher::new(data_dir);
    let mut streams = Vec::new();
    for period in periods {
        let file = ArchiveFile::new(
            symbol,
            Dataset::Klines {
                interval: interval.to_owned(),
            },
            *period,
        );
        let path = fetcher.parquet_path(&file);
        if !path.exists() {
            bail!(
                "no data for {}; run `tape data fetch {symbol} --klines {interval} --from ...` first",
                file.stem()
            );
        }
        let (_, bars) = store::read_bars(&path, InstrumentId(1))?;
        streams.push(bars.map(|bar| bar.map(MarketEvent::Bar)));
    }
    Ok(streams.into_iter().flatten())
}

impl RunArgs {
    pub fn from(&self) -> &str {
        &self.from
    }

    pub fn to(&self) -> Option<&str> {
        self.to.as_deref()
    }
}

fn make_strategy(args: &RunArgs) -> Result<Box<dyn Strategy>> {
    Ok(match args.strategy {
        StrategyName::BuyAndHold => Box::new(BuyAndHold::new(InstrumentId(1))),
        StrategyName::SmaCross => Box::new(SmaCross::new(InstrumentId(1), args.fast, args.slow)),
        StrategyName::Wasm => {
            let path = args.module.as_ref().expect("clap requires --module");
            let bytes =
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            let sandbox = tape_wasm::Sandbox::new(tape_wasm::Limits::default());
            Box::new(
                sandbox
                    .load(bytes)
                    .with_context(|| format!("failed to load {}", path.display()))?,
            )
        }
    })
}

struct Outcome {
    label: &'static str,
    result: BacktestResult,
    metrics: Option<Metrics>,
    benchmark: Option<Metrics>,
    seconds: f64,
}

pub(crate) fn money(value: f64) -> String {
    let cents = (value * 100.0).round() as i64;
    let (whole, frac) = (cents.abs() / 100, cents.abs() % 100);
    let digits = whole.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{}{grouped}.{frac:02}", if cents < 0 { "-" } else { "" })
}

pub(crate) fn pct(value: f64) -> String {
    format!("{:+.2}%", value * 100.0)
}

pub(crate) fn ratio(value: Option<f64>) -> String {
    value.map_or("n/a".into(), |v| format!("{v:.2}"))
}

pub fn run(data_dir: &Path, args: RunArgs, periods: Vec<Period>) -> Result<()> {
    let instrument = instrument(&args.symbol)?;
    let models: Vec<(&'static str, ExecutionModel)> = match args.model {
        Model::Naive => vec![("naive", ExecutionModel::naive())],
        Model::Realistic => vec![("realistic", ExecutionModel::realistic())],
        Model::Both => vec![
            ("naive", ExecutionModel::naive()),
            ("realistic", ExecutionModel::realistic()),
        ],
    };

    let mut outcomes = Vec::new();
    for (label, execution) in models {
        let config = BacktestConfig {
            initial_cash: args.cash,
            execution,
            ..BacktestConfig::default()
        };
        let backtest = Backtest::new(config, [instrument.clone()]);

        let started = Instant::now();
        let mut strategy = make_strategy(&args)?;
        let result = backtest
            .run(
                strategy.as_mut(),
                load(data_dir, &args.symbol, &args.interval, &periods)?,
            )
            .with_context(|| format!("backtest failed under the {label} model"))?;
        let seconds = started.elapsed().as_secs_f64();

        let benchmark = backtest
            .run(
                &mut BuyAndHold::new(InstrumentId(1)),
                load(data_dir, &args.symbol, &args.interval, &periods)?,
            )
            .context("benchmark failed")?;
        outcomes.push(Outcome {
            label,
            metrics: tape_metrics::compute(&result.equity, Periods::DAILY_24_7),
            benchmark: tape_metrics::compute(&benchmark.equity, Periods::DAILY_24_7),
            result,
            seconds,
        });
    }

    print_summary(&args, &outcomes);

    if let Some(path) = &args.json {
        let results: Vec<_> = outcomes
            .iter()
            .map(|o| serde_json::json!({ "model": o.label, "result": o.result, "metrics": o.metrics }))
            .collect();
        fs::write(path, serde_json::to_vec_pretty(&results)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
        eprintln!("wrote {}", path.display());
    }
    Ok(())
}

fn print_summary(args: &RunArgs, outcomes: &[Outcome]) {
    let first = &outcomes[0].result;
    println!("strategy   {}", first.strategy);
    if let Some((start, end)) = first.span {
        println!(
            "data       {} {}  {start} → {end}  ({} events)",
            args.symbol, args.interval, first.events
        );
    }
    println!("capital    {}", money(args.cash.to_f64()));
    println!();

    let width = 18;
    let header: String = outcomes
        .iter()
        .map(|o| format!("{:>width$}", o.label))
        .collect();
    println!("{:<22}{header}", "");
    let row = |name: &str, cell: &dyn Fn(&Outcome) -> String| {
        let cells: String = outcomes
            .iter()
            .map(|o| format!("{:>width$}", cell(o)))
            .collect();
        println!("{name:<22}{cells}");
    };
    let metric =
        |f: fn(&Metrics) -> String| move |o: &Outcome| o.metrics.as_ref().map_or("n/a".into(), f);
    row("final equity", &|o| money(o.result.final_equity.to_f64()));
    row("return", &metric(|m| pct(m.total_return)));
    row("buy & hold return", &|o| {
        o.benchmark
            .as_ref()
            .map_or("n/a".into(), |m| pct(m.total_return))
    });
    row("sharpe (daily)", &metric(|m| ratio(m.sharpe)));
    row("sortino (daily)", &metric(|m| ratio(m.sortino)));
    row("max drawdown", &metric(|m| pct(-m.max_drawdown)));
    row("fills", &|o| o.result.fills.len().to_string());
    row("fees paid", &|o| money(o.result.fees_paid.to_f64()));
    row("events / second", &|o| {
        format!("{:.1}M", o.result.events as f64 / o.seconds / 1e6)
    });
    row("run hash", &|o| o.result.run_hash.clone());
}
