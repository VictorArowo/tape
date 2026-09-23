//! `tape lab run`: the AI research desk.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use clap::Args;
use serde_json::json;
use tape_core::{MarketEvent, Money, TimeDelta, Timestamp};
use tape_data::binance::Period;
use tape_engine::ExecutionModel;
use tape_lab::model::Model;
use tape_lab::{Budget, GuestBuilder, LabConfig, Market, OpenAi, Scripted};
use tape_wasm::Limits;

use crate::run::{instrument, load};

#[derive(Args)]
pub struct LabArgs {
    /// How many ideas to try.
    #[arg(long, default_value_t = 30)]
    attempts: usize,
    /// Hard spending cap in US dollars. The run stops before exceeding it.
    #[arg(long, default_value = "10")]
    budget: f64,
    /// gpt-6-luna or gpt-6-sol.
    #[arg(long, default_value = "gpt-6-luna")]
    model: String,
    /// Attempts in flight at once.
    #[arg(long, default_value_t = 4)]
    workers: usize,
    /// Reasoning effort: none, low, medium, high, xhigh or max.
    #[arg(long, default_value = "low")]
    effort: String,
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    #[arg(long, default_value = "2023-01")]
    pub train_from: String,
    #[arg(long, default_value = "2024-12")]
    pub train_to: String,
    /// First test month. The default is after gpt-6-luna's knowledge cutoff
    /// (18 May 2026), so the model can't have seen what happened.
    #[arg(long, default_value = "2026-06")]
    pub test_from: String,
    #[arg(long, default_value = "2026-08")]
    pub test_to: String,
    /// Months before the test period that strategies see first, so their
    /// indicators are ready; they don't count towards the test results.
    #[arg(long, default_value_t = 4)]
    pub warmup_months: usize,
    #[arg(long, default_value = "10000")]
    cash: Money,
    /// Where to write the run. Defaults to results/lab/<timestamp>.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Use a scripted model that always proposes the reference crossover,
    /// to check the pipeline on real data without spending anything.
    #[arg(long)]
    dry_run: bool,
}

fn events(data_dir: &Path, symbol: &str, periods: &[Period]) -> Result<Vec<MarketEvent>> {
    load(data_dir, symbol, "1m", periods)?
        .collect::<Result<Vec<_>, _>>()
        .context("failed to read market data")
}

fn label(periods: &[Period], last: bool) -> String {
    let period = if last {
        periods.last()
    } else {
        periods.first()
    };
    match period {
        Some(Period::Month { year, month }) if !last => format!("{year:04}-{month:02}-01"),
        Some(Period::Month { year, month }) => {
            let next = if *month == 12 {
                Timestamp::from_ymd(i64::from(*year) + 1, 1, 1)
            } else {
                Timestamp::from_ymd(i64::from(*year), month + 1, 1)
            };
            let (y, m, d) = (next - TimeDelta::from_days(1)).date();
            format!("{y:04}-{m:02}-{d:02}")
        }
        _ => String::new(),
    }
}

fn run_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let ts = Timestamp::from_secs(now);
    let (y, m, d) = ts.date();
    let secs_of_day = now.rem_euclid(86_400);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs_of_day / 3_600,
        secs_of_day % 3_600 / 60,
        secs_of_day % 60
    )
}

#[derive(Args)]
pub struct EvaluateArgs {
    /// A finished run directory, e.g. results/lab/pilot-1.
    #[arg(long)]
    run: PathBuf,
    #[arg(long, default_value = "BTCUSDT")]
    symbol: String,
    #[arg(long, default_value = "2023-01")]
    pub train_from: String,
    #[arg(long, default_value = "2024-12")]
    pub train_to: String,
    /// First test month. The default is after gpt-6-luna's knowledge cutoff
    /// (18 May 2026), so the model can't have seen what happened.
    #[arg(long, default_value = "2026-06")]
    pub test_from: String,
    #[arg(long, default_value = "2026-08")]
    pub test_to: String,
    #[arg(long, default_value_t = 4)]
    pub warmup_months: usize,
    #[arg(long, default_value = "10000")]
    cash: Money,
    /// Where to write the new report. Defaults to
    /// <run>/eval-<test-from>-<test-to>.
    #[arg(long)]
    out: Option<PathBuf>,
}

fn print_summary(report: &tape_lab::Report, out: &Path) {
    let f = &report.funnel;
    println!();
    println!("{}", report.stopped);
    println!(
        "funnel: {} proposed, {} compiled, {} ran, {} passed the critic, {} beat buy-and-hold out of sample",
        f.proposed, f.compiled, f.ran, f.passed_critic, f.beat_buy_hold
    );
    let sharpe = |s: Option<f64>| s.map_or("n/a".into(), |s| format!("{s:.2}"));
    println!(
        "buy and hold: Sharpe {} in sample, {} out of sample",
        sharpe(report.buy_hold_in_sample.sharpe),
        sharpe(report.buy_hold_out_of_sample.sharpe)
    );
    if let Some(d) = &report.deflated {
        println!(
            "best in sample: {} (Sharpe {:.2}); luck alone over {} tries would reach about {:.2}; \
             deflated Sharpe probability {}",
            d.best,
            d.best_in_sample_sharpe,
            d.trials,
            d.luck_threshold_sharpe,
            d.probability
                .map_or("n/a".into(), |p| format!("{:.1}%", p * 100.0))
        );
    }
    println!(
        "spent ${:.4} of ${:.2}",
        report.spent_usd, report.budget_usd
    );
    println!("report: {}", out.join("experiment.json").display());
}

fn market(
    data_dir: &Path,
    symbol: &str,
    train: &[Period],
    test: &[Period],
    warmup: &[Period],
) -> Result<Market> {
    eprintln!(
        "loading {} training months, {} warm-up months and {} test months",
        train.len(),
        warmup.len(),
        test.len()
    );
    Ok(Market {
        instrument: instrument(symbol)?,
        train: tape_lab::Period {
            from: label(train, false),
            to: label(train, true),
            events: events(data_dir, symbol, train)?,
        },
        test: tape_lab::Period {
            from: label(test, false),
            to: label(test, true),
            events: events(data_dir, symbol, test)?,
        },
        warmup: events(data_dir, symbol, warmup)?,
    })
}

pub fn evaluate(
    data_dir: &Path,
    args: EvaluateArgs,
    train: Vec<Period>,
    test: Vec<Period>,
    warmup: Vec<Period>,
) -> Result<()> {
    let market = market(data_dir, &args.symbol, &train, &test, &warmup)?;
    let out = args.out.unwrap_or_else(|| {
        args.run
            .join(format!("eval-{}-{}", args.test_from, args.test_to))
    });
    let config = LabConfig {
        attempts: 0,
        workers: 1,
        max_repairs: 0,
        initial_cash: args.cash,
        execution: ExecutionModel::realistic(),
        limits: Limits::default(),
        out_dir: out.clone(),
    };
    let report = tape_lab::lab::evaluate(&config, &market, &args.run, &|line: &str| {
        eprintln!("{line}")
    })?;
    print_summary(&report, &out);
    Ok(())
}

pub fn lab(
    data_dir: &Path,
    args: LabArgs,
    train: Vec<Period>,
    test: Vec<Period>,
    warmup: Vec<Period>,
) -> Result<()> {
    if args.budget <= 0.0 {
        bail!("--budget must be positive");
    }
    let root = std::env::current_dir()?;
    if !root.join("crates/tape-guest").exists() {
        bail!("run `tape lab` from the root of the tape repository");
    }
    let market = market(data_dir, &args.symbol, &train, &test, &warmup)?;

    let model: Box<dyn Model> = if args.dry_run {
        let code = include_str!("../../../guests/sma-cross/src/lib.rs");
        Box::new(
            Scripted::new(0.0)
                .answer(
                    "hypothesis",
                    [json!({
                        "name": "Reference crossover",
                        "hypothesis": "A dry run: the 5/20 crossover every time.",
                        "rules": "Long when the 5-bar average is above the 20-bar average."
                    })],
                )
                .answer("strategy_code", [json!({"code": code, "notes": "dry run"})])
                .answer(
                    "critique",
                    [json!({"verdict": "reject", "reason": "Dry run.", "concerns": []})],
                ),
        )
    } else {
        Box::new(OpenAi::new(&root, &args.model, &args.effort)?)
    };

    let out = args
        .out
        .unwrap_or_else(|| root.join("results/lab").join(run_id()));
    let config = LabConfig {
        attempts: args.attempts,
        workers: args.workers,
        max_repairs: 2,
        initial_cash: args.cash,
        execution: ExecutionModel::realistic(),
        limits: Limits::default(),
        out_dir: out.clone(),
    };
    let budget = Budget::new(args.budget);
    let builder = GuestBuilder::new(&root)?;
    eprintln!(
        "model {} (effort {}), {} attempts, ${:.2} cap, writing to {}",
        model.name(),
        args.effort,
        args.attempts,
        args.budget,
        out.display()
    );
    let report = tape_lab::run(
        &config,
        &market,
        model.as_ref(),
        &budget,
        &builder,
        &|line: &str| {
            eprintln!("[${:.3}] {line}", budget.spent());
        },
    )?;

    print_summary(&report, &out);
    Ok(())
}
