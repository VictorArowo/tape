//! The research desk: propose, implement, backtest, critique, repeat. Then
//! one honest test.
//!
//! Each attempt runs through the same funnel:
//!
//! 1. The **researcher** proposes an idea, seeing what was tried before
//!    and how it went.
//! 2. The **coder** writes it as a sandboxed strategy. Source that breaks
//!    the rules, or doesn't compile, goes back with the errors, a limited
//!    number of times.
//! 3. The strategy is **backtested** on the training period only, under
//!    realistic execution, and scored on the whole period and on each half.
//! 4. The **critic** sees the idea, the code and those numbers, and passes
//!    or rejects it.
//!
//! The test period is never shown to any agent and never used to choose
//! anything. Only after every attempt is finished is each strategy run on
//! it once, and the survivors compared with buying and holding.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use tape_core::{Instrument, InstrumentId, MarketEvent, Money, TimeDelta, Timestamp};
use tape_engine::{
    Backtest, BacktestConfig, BacktestResult, EquityPoint, ExecutionModel, Strategy,
};
use tape_metrics::{Periods, deflated, period_returns};
use tape_wasm::{Limits, Sandbox};

use crate::build::{BuildError, GuestBuilder};
use crate::model::{Budget, Model, ModelError, Prompt};
use crate::prompts::{self, MarketBrief};

/// A contiguous stretch of bars, with a human-readable label.
pub struct Period {
    pub from: String,
    pub to: String,
    pub events: Vec<MarketEvent>,
}

pub struct Market {
    pub instrument: Instrument,
    pub train: Period,
    pub test: Period,
}

pub struct LabConfig {
    pub attempts: usize,
    /// Extra tries the coder gets after a rejected or failed build.
    pub max_repairs: usize,
    pub initial_cash: Money,
    pub execution: ExecutionModel,
    pub limits: Limits,
    pub out_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum LabError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("benchmark failed: {0}")]
    Benchmark(String),
    #[error("model error: {0}")]
    Model(#[from] ModelError),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Idea {
    pub name: String,
    pub hypothesis: String,
    pub rules: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stats {
    pub from: String,
    pub to: String,
    pub total_return: f64,
    pub sharpe: Option<f64>,
    pub max_drawdown: f64,
    pub trades: usize,
    pub fees: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Critique {
    pub verdict: String,
    pub reason: String,
    pub concerns: Vec<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The researcher or coder couldn't produce anything usable.
    NoCode,
    /// Every build attempt was rejected or failed to compile.
    BuildFailed,
    /// Compiled, but crashed or broke a sandbox limit during the backtest.
    RuntimeFailed,
    Rejected,
    Passed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub idea: Option<Idea>,
    pub status: Status,
    pub builds: usize,
    pub error: Option<String>,
    pub in_sample: Option<Stats>,
    pub halves: Option<[Stats; 2]>,
    pub critique: Option<Critique>,
    /// Filled in only at the very end, after all attempts are done.
    pub out_of_sample: Option<Stats>,
    pub cost_usd: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Funnel {
    pub proposed: usize,
    pub compiled: usize,
    pub ran: usize,
    pub passed_critic: usize,
    pub beat_buy_hold: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeflatedSummary {
    pub best: String,
    pub best_in_sample_sharpe: f64,
    pub trials: usize,
    /// Annualized Sharpe ratio the best of this many skill-less strategies
    /// would be expected to reach by luck.
    pub luck_threshold_sharpe: f64,
    /// Probability that the best strategy's true Sharpe ratio is positive.
    pub probability: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub model: String,
    pub attempts: Vec<Attempt>,
    pub funnel: Funnel,
    pub buy_hold_in_sample: Stats,
    pub buy_hold_out_of_sample: Stats,
    pub deflated: Option<DeflatedSummary>,
    pub spent_usd: f64,
    pub budget_usd: f64,
    pub stopped: String,
}

fn summarize(result: &BacktestResult, equity: &[EquityPoint], from: String, to: String) -> Stats {
    let metrics = tape_metrics::compute(equity, Periods::DAILY_24_7);
    Stats {
        from,
        to,
        total_return: metrics.as_ref().map_or(0.0, |m| m.total_return),
        sharpe: metrics.as_ref().and_then(|m| m.sharpe),
        max_drawdown: metrics.as_ref().map_or(0.0, |m| m.max_drawdown),
        trades: result.fills.len(),
        fees: result.fees_paid.to_f64(),
    }
}

fn date(ts: Timestamp) -> String {
    let (y, m, d) = ts.date();
    format!("{y:04}-{m:02}-{d:02}")
}

fn pct(v: f64) -> String {
    format!("{:+.1}%", v * 100.0)
}

fn sharpe(v: Option<f64>) -> String {
    v.map_or("n/a".into(), |s| format!("{s:.2}"))
}

impl Stats {
    fn line(&self) -> String {
        format!(
            "{} to {}: return {}, Sharpe {}, max drawdown {}, {} fills, {:.0} USDT in fees",
            self.from,
            self.to,
            pct(self.total_return),
            sharpe(self.sharpe),
            pct(-self.max_drawdown),
            self.trades,
            self.fees
        )
    }
}

struct Desk<'a> {
    config: &'a LabConfig,
    market: &'a Market,
    model: &'a dyn Model,
    budget: &'a Budget,
    sandbox: Sandbox,
    /// Middle of the training period, for the two-halves check.
    split: Timestamp,
}

impl Desk<'_> {
    fn backtest(
        &self,
        strategy: &mut dyn Strategy,
        events: &[MarketEvent],
    ) -> Result<BacktestResult, String> {
        let config = BacktestConfig {
            initial_cash: self.config.initial_cash,
            execution: self.config.execution.clone(),
            ..BacktestConfig::default()
        };
        Backtest::new(config, [self.market.instrument.clone()])
            .run(strategy, events.iter().cloned().map(Ok::<_, Infallible>))
            .map_err(|err| err.to_string())
    }

    fn run_wasm(&self, wasm: &[u8], events: &[MarketEvent]) -> Result<BacktestResult, String> {
        let mut strategy = self.sandbox.load(wasm).map_err(|err| err.to_string())?;
        self.backtest(&mut strategy, events)
    }

    fn in_sample(&self, result: &BacktestResult) -> (Stats, [Stats; 2]) {
        let train = &self.market.train;
        let full = summarize(result, &result.equity, train.from.clone(), train.to.clone());
        let (first, second): (Vec<_>, Vec<_>) = result
            .equity
            .iter()
            .partition(|point| point.ts < self.split);
        let last_first = date(self.split - TimeDelta::from_nanos(1));
        // Fills are split the same way so each half reports its own trading.
        let fills_before = result.fills.iter().filter(|f| f.ts < self.split).count();
        let fees_before: f64 = result
            .fills
            .iter()
            .filter(|f| f.ts < self.split)
            .map(|f| f.fee.to_f64())
            .sum();
        let mut a = summarize(result, &first, train.from.clone(), last_first);
        let mut b = summarize(result, &second, date(self.split), train.to.clone());
        a.trades = fills_before;
        a.fees = fees_before;
        b.trades = result.fills.len() - fills_before;
        b.fees = full.fees - fees_before;
        (full, [a, b])
    }

    fn call(&self, prompt: &Prompt) -> Result<(Value, f64), ModelError> {
        let completion = self.budget.complete(self.model, prompt)?;
        Ok((completion.json, completion.cost_usd))
    }
}

fn history_line(attempt: &Attempt) -> Option<String> {
    let idea = attempt.idea.as_ref()?;
    let outcome = match attempt.status {
        Status::NoCode | Status::BuildFailed => "couldn't be built".to_owned(),
        Status::RuntimeFailed => format!(
            "crashed: {}",
            attempt.error.as_deref().unwrap_or("unknown error")
        ),
        Status::Rejected | Status::Passed => {
            let stats = attempt.in_sample.as_ref()?;
            let verdict = attempt.critique.as_ref().map_or(String::new(), |c| {
                format!("; critic: {} ({})", c.verdict, c.reason)
            });
            format!(
                "return {}, Sharpe {}, {} fills{verdict}",
                pct(stats.total_return),
                sharpe(stats.sharpe),
                stats.trades
            )
        }
    };
    Some(format!("- {}: {} -> {outcome}", idea.name, idea.hypothesis))
}

/// Runs the whole experiment. `progress` receives one line per step.
pub fn run(
    config: &LabConfig,
    market: &Market,
    model: &dyn Model,
    budget: &Budget,
    builder: &GuestBuilder,
    progress: &mut dyn FnMut(&str),
) -> Result<Report, LabError> {
    fs::create_dir_all(config.out_dir.join("code"))?;
    fs::create_dir_all(config.out_dir.join("wasm"))?;
    let mut log = File::create(config.out_dir.join("attempts.jsonl"))?;

    let first = market
        .train
        .events
        .first()
        .map_or(Timestamp::EPOCH, MarketEvent::ts);
    let last = market
        .train
        .events
        .last()
        .map_or(Timestamp::EPOCH, MarketEvent::ts);
    let split = first + TimeDelta::from_nanos((last - first).as_nanos() / 2);
    let split = Timestamp::from_ymd(split.date().0, split.date().1, 1);
    let desk = Desk {
        config,
        market,
        model,
        budget,
        sandbox: Sandbox::new(config.limits.clone()),
        split,
    };

    let benchmark =
        |events: &[MarketEvent], from: &str, to: &str| -> Result<(Stats, Vec<f64>), LabError> {
            let mut strategy = tape_strategies_buy_and_hold(market.instrument.id);
            let result = desk
                .backtest(strategy.as_mut(), events)
                .map_err(LabError::Benchmark)?;
            let returns = period_returns(&result.equity, TimeDelta::from_days(1));
            Ok((
                summarize(&result, &result.equity, from.into(), to.into()),
                returns,
            ))
        };
    let (buy_hold_in_sample, _) =
        benchmark(&market.train.events, &market.train.from, &market.train.to)?;
    let brief = MarketBrief {
        symbol: market.instrument.symbol.clone(),
        train_from: market.train.from.clone(),
        train_to: market.train.to.clone(),
        buy_hold_return: buy_hold_in_sample.total_return,
        buy_hold_sharpe: buy_hold_in_sample.sharpe,
        initial_cash: config.initial_cash.to_f64(),
    };
    let coder_system = prompts::coder_system();

    let mut attempts: Vec<Attempt> = Vec::new();
    let mut modules: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut daily_returns: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut stopped = format!("finished all {} attempts", config.attempts);

    'attempts: for n in 1..=config.attempts {
        let id = format!("s{n:03}");
        let mut attempt = Attempt {
            id: id.clone(),
            idea: None,
            status: Status::NoCode,
            builds: 0,
            error: None,
            in_sample: None,
            halves: None,
            critique: None,
            out_of_sample: None,
            cost_usd: 0.0,
        };

        // Every model call either succeeds, fails this attempt, or (if the
        // budget is gone) ends the run.
        macro_rules! ask {
            ($prompt:expr) => {
                match desk.call(&$prompt) {
                    Ok((json, cost)) => {
                        attempt.cost_usd += cost;
                        Some(json)
                    }
                    Err(ModelError::BudgetExhausted { cap, spent }) => {
                        stopped =
                            format!("stopped at the ${cap:.2} budget after spending ${spent:.2}");
                        break 'attempts;
                    }
                    Err(
                        err @ (ModelError::MissingKey
                        | ModelError::Http {
                            status: 401 | 403, ..
                        }),
                    ) => {
                        return Err(err.into());
                    }
                    Err(err) => {
                        attempt.error = Some(err.to_string());
                        None
                    }
                }
            };
        }

        let history: String = attempts
            .iter()
            .rev()
            .take(40)
            .rev()
            .filter_map(history_line)
            .collect::<Vec<_>>()
            .join("\n");
        let researched = ask!(prompts::researcher(&brief, &history));
        let idea: Option<Idea> = researched.and_then(|json| serde_json::from_value(json).ok());
        let Some(idea) = idea else {
            progress(&format!("{id} no usable idea"));
            record(&mut log, &attempt)?;
            attempts.push(attempt);
            continue;
        };
        attempt.idea = Some(idea.clone());
        progress(&format!("{id} idea: {}", idea.name));
        let idea_json = serde_json::to_value(&idea).unwrap_or_default();

        // Write, check and compile, with a few chances to fix mistakes.
        let mut wasm = None;
        let mut prompt = prompts::coder(&coder_system, &idea_json);
        for _ in 0..=config.max_repairs {
            let Some(answer) = ask!(prompt) else { break };
            let Some(code) = answer["code"].as_str().map(str::to_owned) else {
                attempt.error = Some("coder returned no code".into());
                break;
            };
            attempt.builds += 1;
            fs::write(config.out_dir.join("code").join(format!("{id}.rs")), &code)?;
            match builder.build(&code) {
                Ok(module) => {
                    wasm = Some(module);
                    attempt.error = None;
                    break;
                }
                Err(err @ (BuildError::Rejected(_) | BuildError::Compile(_))) => {
                    progress(&format!(
                        "{id} build {}: {}",
                        attempt.builds,
                        first_line(&err.to_string())
                    ));
                    attempt.error = Some(err.to_string());
                    prompt = prompts::repair(&coder_system, &idea_json, &code, &err.to_string());
                }
                Err(err) => {
                    attempt.error = Some(err.to_string());
                    break;
                }
            }
        }
        let Some(wasm) = wasm else {
            attempt.status = if attempt.builds == 0 {
                Status::NoCode
            } else {
                Status::BuildFailed
            };
            record(&mut log, &attempt)?;
            attempts.push(attempt);
            continue;
        };
        fs::write(
            config.out_dir.join("wasm").join(format!("{id}.wasm")),
            &wasm,
        )?;

        // Backtest on the training period only.
        let result = match desk.run_wasm(&wasm, &market.train.events) {
            Ok(result) => result,
            Err(err) => {
                progress(&format!("{id} failed at runtime: {}", first_line(&err)));
                attempt.status = Status::RuntimeFailed;
                attempt.error = Some(err);
                record(&mut log, &attempt)?;
                attempts.push(attempt);
                continue;
            }
        };
        let (stats, halves) = desk.in_sample(&result);
        daily_returns.insert(
            id.clone(),
            period_returns(&result.equity, TimeDelta::from_days(1)),
        );
        modules.insert(id.clone(), wasm);
        progress(&format!("{id} in-sample {}", stats.line()));
        attempt.in_sample = Some(stats.clone());
        attempt.halves = Some(halves.clone());

        let code = fs::read_to_string(config.out_dir.join("code").join(format!("{id}.rs")))?;
        let evidence = format!(
            "Idea:\n{}\n\nCode:\n```rust\n{code}\n```\n\nBacktest results (training data only):\n\
             Whole period: {}\nFirst half: {}\nSecond half: {}\n\n\
             Buying and holding over the whole period: {}",
            serde_json::to_string_pretty(&idea_json).unwrap_or_default(),
            stats.line(),
            halves[0].line(),
            halves[1].line(),
            buy_hold_in_sample.line(),
        );
        let critique: Option<Critique> =
            ask!(prompts::critic(&evidence)).and_then(|json| serde_json::from_value(json).ok());
        attempt.status = match &critique {
            Some(c) if c.verdict == "pass" => Status::Passed,
            _ => Status::Rejected,
        };
        progress(&format!(
            "{id} critic: {}",
            critique.as_ref().map_or("no verdict".into(), |c| format!(
                "{} ({})",
                c.verdict, c.reason
            ))
        ));
        attempt.critique = critique;
        record(&mut log, &attempt)?;
        attempts.push(attempt);
    }

    // The one look at the test period, after every decision has been made.
    progress("running every strategy once on the held-out test period");
    let (buy_hold_out_of_sample, _) =
        benchmark(&market.test.events, &market.test.from, &market.test.to)?;
    for attempt in &mut attempts {
        let Some(wasm) = modules.get(&attempt.id) else {
            continue;
        };
        if let Ok(result) = desk.run_wasm(wasm, &market.test.events) {
            attempt.out_of_sample = Some(summarize(
                &result,
                &result.equity,
                market.test.from.clone(),
                market.test.to.clone(),
            ));
        }
    }

    let ran: Vec<&Attempt> = attempts.iter().filter(|a| a.in_sample.is_some()).collect();
    let bh_oos = buy_hold_out_of_sample.sharpe.unwrap_or(0.0);
    let funnel = Funnel {
        proposed: attempts.iter().filter(|a| a.idea.is_some()).count(),
        compiled: attempts
            .iter()
            .filter(|a| {
                matches!(
                    a.status,
                    Status::RuntimeFailed | Status::Rejected | Status::Passed
                )
            })
            .count(),
        ran: ran.len(),
        passed_critic: attempts
            .iter()
            .filter(|a| a.status == Status::Passed)
            .count(),
        beat_buy_hold: attempts
            .iter()
            .filter(|a| a.status == Status::Passed)
            .filter(|a| {
                a.out_of_sample
                    .as_ref()
                    .and_then(|s| s.sharpe)
                    .is_some_and(|s| s > bh_oos)
            })
            .count(),
    };
    let deflated = deflated_summary(&ran, &daily_returns);

    let report = Report {
        model: model.name().to_owned(),
        attempts,
        funnel,
        buy_hold_in_sample,
        buy_hold_out_of_sample,
        deflated,
        spent_usd: budget.spent(),
        budget_usd: budget.cap(),
        stopped,
    };
    fs::write(
        config.out_dir.join("report.json"),
        serde_json::to_vec_pretty(&report).unwrap_or_default(),
    )?;
    fs::write(
        config.out_dir.join("experiment.json"),
        serde_json::to_vec_pretty(&report.experiment_json(market)).unwrap_or_default(),
    )?;
    Ok(report)
}

fn tape_strategies_buy_and_hold(instrument: InstrumentId) -> Box<dyn Strategy> {
    Box::new(BuyAndHoldOnce {
        instrument,
        bought: false,
    })
}

/// A local copy of the reference buy-and-hold, so the lab doesn't depend
/// on the strategies crate.
struct BuyAndHoldOnce {
    instrument: InstrumentId,
    bought: bool,
}

impl Strategy for BuyAndHoldOnce {
    fn name(&self) -> String {
        "buy-and-hold".into()
    }

    fn on_market(&mut self, ctx: &mut tape_engine::Context<'_>, event: &MarketEvent) {
        if self.bought || event.instrument() != self.instrument {
            return;
        }
        let Some(price) = ctx.last_price(self.instrument) else {
            return;
        };
        let qty = ctx.affordable_qty(self.instrument, price, ctx.cash(), 5_000);
        if qty.is_positive() {
            ctx.submit(tape_core::OrderRequest::market(
                self.instrument,
                tape_core::Side::Buy,
                qty,
            ));
            self.bought = true;
        }
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.chars().take(160).collect()
}

fn record(log: &mut File, attempt: &Attempt) -> std::io::Result<()> {
    serde_json::to_writer(&mut *log, attempt)?;
    log.write_all(b"\n")?;
    log.flush()
}

fn deflated_summary(
    ran: &[&Attempt],
    daily_returns: &BTreeMap<String, Vec<f64>>,
) -> Option<DeflatedSummary> {
    let scale = 365f64.sqrt();
    let sharpes: Vec<(&str, f64)> = ran
        .iter()
        .filter_map(|a| Some((a.id.as_str(), a.in_sample.as_ref()?.sharpe?)))
        .collect();
    let (best, best_sharpe) = sharpes.iter().copied().max_by(|a, b| a.1.total_cmp(&b.1))?;
    let per_period: Vec<f64> = sharpes.iter().map(|(_, s)| s / scale).collect();
    let mean = per_period.iter().sum::<f64>() / per_period.len() as f64;
    let variance = if per_period.len() > 1 {
        per_period.iter().map(|s| (s - mean).powi(2)).sum::<f64>() / (per_period.len() - 1) as f64
    } else {
        0.0
    };
    let threshold = deflated::expected_max_sharpe(per_period.len(), variance);
    Some(DeflatedSummary {
        best: best.to_owned(),
        best_in_sample_sharpe: best_sharpe,
        trials: per_period.len(),
        luck_threshold_sharpe: threshold * scale,
        probability: deflated::deflated_sharpe(
            best_sharpe / scale,
            daily_returns.get(best)?,
            per_period.len(),
            variance,
        ),
    })
}

impl Report {
    /// The experiment in the format the project site reads.
    pub fn experiment_json(&self, market: &Market) -> Value {
        let round = |v: f64| (v * 100.0).round() / 100.0;
        let passed: Vec<&Attempt> = self
            .attempts
            .iter()
            .filter(|a| a.status == Status::Passed)
            .collect();
        let point = |a: &Attempt| {
            json!({
                "id": a.id,
                "name": a.idea.as_ref().map(|i| i.name.clone()),
                "inSampleSharpe": a.in_sample.as_ref().and_then(|s| s.sharpe).map(round),
                "outOfSampleSharpe": a.out_of_sample.as_ref().and_then(|s| s.sharpe).map(round),
                "status": a.status,
            })
        };
        let mut by_in_sample: Vec<&Attempt> = if passed.is_empty() {
            self.attempts
                .iter()
                .filter(|a| a.in_sample.is_some())
                .collect()
        } else {
            passed.clone()
        };
        by_in_sample.sort_by(|a, b| {
            let s = |x: &Attempt| {
                x.in_sample
                    .as_ref()
                    .and_then(|s| s.sharpe)
                    .unwrap_or(f64::MIN)
            };
            s(b).total_cmp(&s(a))
        });
        let examples: Vec<Value> = by_in_sample
            .iter()
            .take(3)
            .map(|a| {
                json!({
                    "name": a.idea.as_ref().map(|i| i.name.clone()),
                    "hypothesis": a.idea.as_ref().map(|i| i.hypothesis.clone()),
                    "inSampleSharpe": a.in_sample.as_ref().and_then(|s| s.sharpe).map(round),
                    "outOfSampleSharpe": a.out_of_sample.as_ref().and_then(|s| s.sharpe).map(round),
                    "critic": a.critique.as_ref().map(|c| c.reason.clone()),
                })
            })
            .collect();
        json!({
            "placeholder": false,
            "model": self.model,
            "train": {"from": market.train.from, "to": market.train.to},
            "test": {"from": market.test.from, "to": market.test.to},
            "funnel": {
                "proposed": self.funnel.proposed,
                "compiled": self.funnel.compiled,
                "ran": self.funnel.ran,
                "passedCritic": self.funnel.passed_critic,
                "beatBuyHold": self.funnel.beat_buy_hold,
            },
            "buyHoldInSampleSharpe": self.buy_hold_in_sample.sharpe.map(round),
            "buyHoldOutOfSampleSharpe": self.buy_hold_out_of_sample.sharpe.map(round),
            "strategies": passed.iter().map(|a| point(a)).collect::<Vec<_>>(),
            "allStrategies": self.attempts.iter().filter(|a| a.in_sample.is_some()).map(point).collect::<Vec<_>>(),
            "examples": examples,
            "deflatedSharpe": self.deflated.as_ref().map(|d| json!({
                "best": d.best,
                "bestInSampleSharpe": round(d.best_in_sample_sharpe),
                "trials": d.trials,
                "luckThresholdSharpe": round(d.luck_threshold_sharpe),
                "probability": d.probability.map(|p| (p * 1000.0).round() / 1000.0),
            })),
            "spentUsd": (self.spent_usd * 100.0).round() / 100.0,
            "stopped": self.stopped,
        })
    }
}
