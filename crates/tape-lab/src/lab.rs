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
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    /// Bars from just before the test period. Strategies see them first so
    /// their indicators are ready when the test starts, but nothing that
    /// happens during the warm-up counts towards the test results.
    pub warmup: Vec<MarketEvent>,
}

pub struct LabConfig {
    pub attempts: usize,
    /// Attempts in flight at once.
    pub workers: usize,
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
    #[error("{0}")]
    Run(String),
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

/// Stats for the part of a run from `start` on, measured from the equity
/// at that moment.
fn summarize_from(result: &BacktestResult, start: Timestamp, from: String, to: String) -> Stats {
    let first = result.equity.partition_point(|point| point.ts < start);
    let window = &result.equity[first.saturating_sub(1)..];
    let fills = result.fills.iter().filter(|f| f.ts >= start);
    let mut stats = summarize(result, window, from, to);
    stats.trades = fills.clone().count();
    stats.fees = fills.map(|f| f.fee.to_f64()).sum();
    stats
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
    sandbox: Sandbox,
    /// Middle of the training period, for the two-halves check.
    split: Timestamp,
}

impl<'a> Desk<'a> {
    fn new(config: &'a LabConfig, market: &'a Market) -> Self {
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
        let middle = first + TimeDelta::from_nanos((last - first).as_nanos() / 2);
        let (year, month, _) = middle.date();
        Self {
            config,
            market,
            sandbox: Sandbox::new(config.limits.clone()),
            split: Timestamp::from_ymd(year, month, 1),
        }
    }

    fn buy_hold(&self, events: &[MarketEvent], from: &str, to: &str) -> Result<Stats, LabError> {
        let mut strategy = tape_strategies_buy_and_hold(self.market.instrument.id);
        let result = self
            .backtest(strategy.as_mut(), events)
            .map_err(LabError::Benchmark)?;
        Ok(summarize(&result, &result.equity, from.into(), to.into()))
    }

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

/// Why an attempt stopped short of an answer from the model.
enum Halt {
    /// The spending cap was reached: finish the run with what there is.
    Budget(String),
    /// Nothing will work (for example, a bad API key): abort the run.
    Fatal(LabError),
}

/// Everything an attempt needs, shared read-only by the workers.
struct Job<'a> {
    config: &'a LabConfig,
    market: &'a Market,
    model: &'a dyn Model,
    budget: &'a Budget,
    builder: &'a GuestBuilder,
    desk: &'a Desk<'a>,
    brief: MarketBrief,
    coder_system: String,
    buy_hold_in_sample: &'a Stats,
}

/// What a finished attempt adds to the run, besides its record.
struct Ran {
    wasm: Vec<u8>,
    daily_returns: Vec<f64>,
}

impl Job<'_> {
    /// One model call. `Ok(None)` means this call failed but the run can go on.
    fn ask(&self, attempt: &mut Attempt, prompt: &Prompt) -> Result<Option<Value>, Halt> {
        match self.budget.complete(self.model, prompt) {
            Ok(completion) => {
                attempt.cost_usd += completion.cost_usd;
                Ok(Some(completion.json))
            }
            Err(ModelError::BudgetExhausted { cap, spent }) => Err(Halt::Budget(format!(
                "stopped at the ${cap:.2} budget after spending ${spent:.2}"
            ))),
            Err(
                err @ (ModelError::MissingKey
                | ModelError::Http {
                    status: 401 | 403, ..
                }),
            ) => Err(Halt::Fatal(err.into())),
            Err(err) => {
                attempt.error = Some(err.to_string());
                Ok(None)
            }
        }
    }

    /// Idea, code, build, in-sample backtest and critique for one attempt.
    fn attempt(
        &self,
        id: &str,
        history: &str,
        say: &(dyn Fn(&str) + Sync),
    ) -> Result<(Attempt, Option<Ran>), Halt> {
        let io = |err: std::io::Error| Halt::Fatal(err.into());
        let out = &self.config.out_dir;
        let mut attempt = Attempt {
            id: id.to_owned(),
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

        let researched = self.ask(&mut attempt, &prompts::researcher(&self.brief, history))?;
        let Some(idea) = researched.and_then(|json| serde_json::from_value::<Idea>(json).ok())
        else {
            say(&format!("{id} no usable idea"));
            return Ok((attempt, None));
        };
        attempt.idea = Some(idea.clone());
        say(&format!("{id} idea: {}", idea.name));
        let idea_json = serde_json::to_value(&idea).unwrap_or_default();

        // Write, check and compile, with a few chances to fix mistakes.
        let code_path = out.join("code").join(format!("{id}.rs"));
        let mut built = None;
        let mut prompt = prompts::coder(&self.coder_system, &idea_json);
        for _ in 0..=self.config.max_repairs {
            let Some(answer) = self.ask(&mut attempt, &prompt)? else {
                break;
            };
            let Some(code) = answer["code"].as_str().map(str::to_owned) else {
                attempt.error = Some("coder returned no code".into());
                break;
            };
            attempt.builds += 1;
            fs::write(&code_path, &code).map_err(io)?;
            match self.builder.build(&code) {
                Ok(module) => {
                    built = Some((module, code));
                    attempt.error = None;
                    break;
                }
                Err(err @ (BuildError::Rejected(_) | BuildError::Compile(_))) => {
                    say(&format!(
                        "{id} build {}: {}",
                        attempt.builds,
                        first_line(&err.to_string())
                    ));
                    attempt.error = Some(err.to_string());
                    prompt =
                        prompts::repair(&self.coder_system, &idea_json, &code, &err.to_string());
                }
                Err(err) => {
                    attempt.error = Some(err.to_string());
                    break;
                }
            }
        }
        let Some((wasm, code)) = built else {
            attempt.status = if attempt.builds == 0 {
                Status::NoCode
            } else {
                Status::BuildFailed
            };
            return Ok((attempt, None));
        };
        fs::write(out.join("wasm").join(format!("{id}.wasm")), &wasm).map_err(io)?;

        // Backtest on the training period only.
        let result = match self.desk.run_wasm(&wasm, &self.market.train.events) {
            Ok(result) => result,
            Err(err) => {
                say(&format!("{id} failed at runtime: {}", first_line(&err)));
                attempt.status = Status::RuntimeFailed;
                attempt.error = Some(err);
                return Ok((attempt, None));
            }
        };
        let (stats, halves) = self.desk.in_sample(&result);
        say(&format!("{id} in-sample {}", stats.line()));
        attempt.in_sample = Some(stats.clone());
        attempt.halves = Some(halves.clone());
        let ran = Ran {
            wasm,
            daily_returns: period_returns(&result.equity, TimeDelta::from_days(1)),
        };

        let evidence = format!(
            "Idea:\n{}\n\nCode:\n```rust\n{code}\n```\n\nBacktest results (training data only):\n\
             Whole period: {}\nFirst half: {}\nSecond half: {}\n\n\
             Buying and holding over the whole period: {}",
            serde_json::to_string_pretty(&idea_json).unwrap_or_default(),
            stats.line(),
            halves[0].line(),
            halves[1].line(),
            self.buy_hold_in_sample.line(),
        );
        let critique: Option<Critique> = self
            .ask(&mut attempt, &prompts::critic(&evidence))?
            .and_then(|json| serde_json::from_value(json).ok());
        attempt.status = match &critique {
            Some(c) if c.verdict == "pass" => Status::Passed,
            _ => Status::Rejected,
        };
        say(&format!(
            "{id} critic: {}",
            critique.as_ref().map_or("no verdict".into(), |c| format!(
                "{} ({})",
                c.verdict, c.reason
            ))
        ));
        attempt.critique = critique;
        Ok((attempt, Some(ran)))
    }
}

/// State the workers share while a run is in progress.
struct Shared {
    attempts: Vec<Attempt>,
    modules: BTreeMap<String, Vec<u8>>,
    daily_returns: BTreeMap<String, Vec<f64>>,
    log: File,
    next: usize,
    stopped: Option<String>,
    fatal: Option<LabError>,
}

impl Shared {
    /// The last 40 finished attempts, for the researcher.
    fn history(&self) -> String {
        let skip = self.attempts.len().saturating_sub(40);
        self.attempts[skip..]
            .iter()
            .filter_map(history_line)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Runs the whole experiment. `progress` receives one line per step.
///
/// `config.workers` attempts run at once. Each researcher call sees every
/// attempt that has finished so far; builds take turns, and the budget
/// reserves each call's worst-case cost up front so parallel calls can't
/// overshoot the cap together.
pub fn run(
    config: &LabConfig,
    market: &Market,
    model: &dyn Model,
    budget: &Budget,
    builder: &GuestBuilder,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Report, LabError> {
    fs::create_dir_all(config.out_dir.join("code"))?;
    fs::create_dir_all(config.out_dir.join("wasm"))?;
    let log = File::create(config.out_dir.join("attempts.jsonl"))?;

    let desk = Desk::new(config, market);
    let buy_hold_in_sample =
        desk.buy_hold(&market.train.events, &market.train.from, &market.train.to)?;
    let job = Job {
        config,
        market,
        model,
        budget,
        builder,
        desk: &desk,
        brief: MarketBrief {
            symbol: market.instrument.symbol.clone(),
            train_from: market.train.from.clone(),
            train_to: market.train.to.clone(),
            buy_hold_return: buy_hold_in_sample.total_return,
            buy_hold_sharpe: buy_hold_in_sample.sharpe,
            initial_cash: config.initial_cash.to_f64(),
        },
        coder_system: prompts::coder_system(),
        buy_hold_in_sample: &buy_hold_in_sample,
    };

    let shared = Mutex::new(Shared {
        attempts: Vec::new(),
        modules: BTreeMap::new(),
        daily_returns: BTreeMap::new(),
        log,
        next: 1,
        stopped: None,
        fatal: None,
    });
    let lock = || {
        shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    };

    std::thread::scope(|scope| {
        for _ in 0..config.workers.max(1) {
            scope.spawn(|| {
                loop {
                    let (id, history) = {
                        let mut state = lock();
                        if state.stopped.is_some()
                            || state.fatal.is_some()
                            || state.next > config.attempts
                        {
                            break;
                        }
                        let id = format!("s{:03}", state.next);
                        state.next += 1;
                        (id, state.history())
                    };
                    match job.attempt(&id, &history, progress) {
                        Ok((attempt, ran)) => {
                            let mut state = lock();
                            if let Some(ran) = ran {
                                state.modules.insert(id.clone(), ran.wasm);
                                state.daily_returns.insert(id, ran.daily_returns);
                            }
                            if let Err(err) = record(&mut state.log, &attempt) {
                                state.fatal.get_or_insert(err.into());
                            }
                            state.attempts.push(attempt);
                        }
                        Err(Halt::Budget(reason)) => {
                            lock().stopped.get_or_insert(reason);
                        }
                        Err(Halt::Fatal(err)) => {
                            lock().fatal.get_or_insert(err);
                        }
                    }
                }
            });
        }
    });

    let state = shared
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(err) = state.fatal {
        return Err(err);
    }
    let mut attempts = state.attempts;
    attempts.sort_by(|a, b| a.id.cmp(&b.id));
    let stopped = state
        .stopped
        .unwrap_or_else(|| format!("finished all {} attempts", config.attempts));
    finalize(
        &desk,
        attempts,
        &state.modules,
        &state.daily_returns,
        buy_hold_in_sample,
        Spend {
            model: model.name().to_owned(),
            spent_usd: budget.spent(),
            budget_usd: budget.cap(),
            stopped,
        },
        progress,
    )
}

/// What a run cost and why it ended, carried into the report.
struct Spend {
    model: String,
    spent_usd: f64,
    budget_usd: f64,
    stopped: String,
}

/// The one look at the test period, after every decision has been made,
/// then the funnel, the luck check and the report files.
fn finalize(
    desk: &Desk<'_>,
    mut attempts: Vec<Attempt>,
    modules: &BTreeMap<String, Vec<u8>>,
    daily_returns: &BTreeMap<String, Vec<f64>>,
    buy_hold_in_sample: Stats,
    spend: Spend,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Report, LabError> {
    let (config, market) = (desk.config, desk.market);
    progress("running every strategy once on the held-out test period");
    let buy_hold_out_of_sample =
        desk.buy_hold(&market.test.events, &market.test.from, &market.test.to)?;
    let test_start = market
        .test
        .events
        .first()
        .map_or(Timestamp::MAX, MarketEvent::ts);
    let warmed: Vec<MarketEvent> = market
        .warmup
        .iter()
        .filter(|event| event.ts() < test_start)
        .chain(&market.test.events)
        .cloned()
        .collect();
    for attempt in &mut attempts {
        attempt.out_of_sample = None;
        let Some(wasm) = modules.get(&attempt.id) else {
            continue;
        };
        if let Ok(result) = desk.run_wasm(wasm, &warmed) {
            // A strategy that lost most of its capital during the warm-up is
            // broken; scoring what its leftover dust does in the test period
            // would only measure noise.
            let at_start = result
                .equity
                .iter()
                .take_while(|point| point.ts < test_start)
                .last()
                .map_or(config.initial_cash, |point| point.equity);
            if at_start < config.initial_cash.mul_ratio(1, 10) {
                attempt.error =
                    Some("lost over 90% during the warm-up, before the test period began".into());
                continue;
            }
            attempt.out_of_sample = Some(summarize_from(
                &result,
                test_start,
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
    let deflated = deflated_summary(&ran, daily_returns);

    let report = Report {
        model: spend.model,
        attempts,
        funnel,
        buy_hold_in_sample,
        buy_hold_out_of_sample,
        deflated,
        spent_usd: spend.spent_usd,
        budget_usd: spend.budget_usd,
        stopped: spend.stopped,
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

/// Re-scores a finished run from its saved strategies, without calling any
/// model: reruns every backtest and the held-out test (for example on a
/// different test period, or after a fix to the evaluation). The training
/// period must match the original run, since the critic judged it.
pub fn evaluate(
    config: &LabConfig,
    market: &Market,
    run_dir: &Path,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Report, LabError> {
    let old: Report = serde_json::from_slice(&fs::read(run_dir.join("report.json"))?)
        .map_err(|err| LabError::Run(format!("unreadable report.json: {err}")))?;
    if (
        old.buy_hold_in_sample.from.as_str(),
        old.buy_hold_in_sample.to.as_str(),
    ) != (market.train.from.as_str(), market.train.to.as_str())
    {
        return Err(LabError::Run(format!(
            "the run was trained on {} to {}; evaluate it with the same training period",
            old.buy_hold_in_sample.from, old.buy_hold_in_sample.to
        )));
    }
    fs::create_dir_all(&config.out_dir)?;
    let desk = Desk::new(config, market);
    let buy_hold_in_sample =
        desk.buy_hold(&market.train.events, &market.train.from, &market.train.to)?;

    let mut modules = BTreeMap::new();
    let mut daily_returns = BTreeMap::new();
    let mut attempts = old.attempts;
    for attempt in &mut attempts {
        if attempt.in_sample.is_none() {
            continue;
        }
        let wasm = fs::read(run_dir.join("wasm").join(format!("{}.wasm", attempt.id)))?;
        match desk.run_wasm(&wasm, &market.train.events) {
            Ok(result) => {
                let (stats, halves) = desk.in_sample(&result);
                attempt.in_sample = Some(stats);
                attempt.halves = Some(halves);
                daily_returns.insert(
                    attempt.id.clone(),
                    period_returns(&result.equity, TimeDelta::from_days(1)),
                );
                modules.insert(attempt.id.clone(), wasm);
            }
            Err(err) => progress(&format!("{} no longer runs: {err}", attempt.id)),
        }
    }
    progress(&format!(
        "re-scored {} strategies on the training period",
        modules.len()
    ));
    finalize(
        &desk,
        attempts,
        &modules,
        &daily_returns,
        buy_hold_in_sample,
        Spend {
            model: old.model,
            spent_usd: old.spent_usd,
            budget_usd: old.budget_usd,
            stopped: old.stopped,
        },
        progress,
    )
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

/// Variance estimated from the median absolute deviation (scaled to match
/// the standard deviation of a normal distribution). A few broken strategies,
/// such as one that trades every minute into a Sharpe of -18, would dominate
/// the ordinary variance and make the luck threshold meaninglessly high.
fn robust_variance(values: &[f64]) -> f64 {
    fn median(sorted: &[f64]) -> f64 {
        let n = sorted.len();
        if n % 2 == 1 {
            sorted[n / 2]
        } else {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        }
    }
    if values.len() < 2 {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let center = median(&sorted);
    let mut deviations: Vec<f64> = sorted.iter().map(|v| (v - center).abs()).collect();
    deviations.sort_by(f64::total_cmp);
    (1.4826 * median(&deviations)).powi(2)
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
    let variance = robust_variance(&per_period);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robust_variance_ignores_a_few_broken_strategies() {
        let normal = [0.9, 1.1, 1.0, 1.2, 0.8, 1.05, 0.95];
        let mut with_bugs = normal.to_vec();
        with_bugs.extend([-18.0, -9.8]);
        let clean = robust_variance(&normal);
        let dirty = robust_variance(&with_bugs);
        assert!(dirty < clean * 3.0, "{dirty} vs {clean}");
        assert_eq!(robust_variance(&[1.0]), 0.0);
    }
}
