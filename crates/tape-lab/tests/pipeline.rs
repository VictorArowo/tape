//! The whole research pipeline, end to end, with a scripted model so it
//! costs nothing: ideas, a build that fails and gets repaired, backtests,
//! critiques, the held-out test and the report.

use std::path::Path;

use serde_json::json;
use tape_core::{
    Bar, Instrument, InstrumentId, MarketEvent, Money, Price, Qty, TimeDelta, Timestamp,
};
use tape_engine::ExecutionModel;
use tape_lab::lab::Status;
use tape_lab::{Budget, GuestBuilder, LabConfig, Market, Period, Scripted, run};
use tape_wasm::Limits;

const BTC: InstrumentId = InstrumentId(1);

fn bars(start: Timestamp, count: usize, seed: u64) -> Vec<MarketEvent> {
    let mut state = seed;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut price = 4_200_000_000_000_i64;
    (0..count)
        .map(|i| {
            let open = price;
            let close =
                (open + (next() % 60_001) as i64 * 1_000_000 - 30_000_000_000).max(100_000_000);
            let high = open.max(close) + (next() % 5_000) as i64 * 1_000_000;
            let low = (open.min(close) - (next() % 5_000) as i64 * 1_000_000).max(1_000_000);
            price = close;
            let open_ts = start + TimeDelta::from_mins(i as i64);
            MarketEvent::Bar(Bar {
                open_ts,
                close_ts: open_ts + TimeDelta::from_mins(1),
                instrument: BTC,
                open: Price::from_raw(open),
                high: Price::from_raw(high),
                low: Price::from_raw(low),
                close: Price::from_raw(close),
                volume: Qty::from_raw((next() % 50_000 + 1_000) as i64 * 100_000),
            })
        })
        .collect()
}

#[test]
fn a_scripted_experiment_runs_end_to_end() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let builder = GuestBuilder::new(&root).unwrap();
    let out = tempfile::tempdir().unwrap();

    let train_start = Timestamp::from_ymd(2024, 1, 1);
    let test_start = train_start + TimeDelta::from_mins(4_000);
    let market = Market {
        instrument: Instrument::new(
            BTC,
            "BTCUSDT",
            "0.01".parse().unwrap(),
            "0.00001".parse().unwrap(),
        ),
        train: Period {
            from: "2024-01-01".into(),
            to: "2024-01-03".into(),
            events: bars(train_start, 4_000, 7),
        },
        test: Period {
            from: "2024-01-03".into(),
            to: "2024-01-04".into(),
            events: bars(test_start, 1_500, 11),
        },
        warmup: Vec::new(),
    };

    let working = include_str!("../../../guests/sma-cross/src/lib.rs");
    let broken = "use tape_guest::*;\nexport_strategy!(DoesNotExist);\n";
    let model = Scripted::new(0.01)
        .answer(
            "hypothesis",
            [
                json!({"name": "Fast crossover", "hypothesis": "Trends persist.", "rules": "5/20 SMA"}),
                json!({"name": "Same again", "hypothesis": "Still trends.", "rules": "5/20 SMA"}),
            ],
        )
        .answer(
            "strategy_code",
            [json!({"code": broken, "notes": ""}), json!({"code": working, "notes": ""})],
        )
        .answer(
            "critique",
            [
                json!({"verdict": "pass", "reason": "Looks fine.", "concerns": []}),
                json!({"verdict": "reject", "reason": "Duplicate idea.", "concerns": ["same as s001"]}),
            ],
        );
    let config = LabConfig {
        attempts: 2,
        workers: 1,
        max_repairs: 2,
        initial_cash: Money::from_int(10_000).unwrap(),
        execution: ExecutionModel::realistic(),
        limits: Limits::default(),
        out_dir: out.path().to_owned(),
    };
    let budget = Budget::new(1.0);
    let lines = std::sync::Mutex::new(Vec::new());
    let report = run(
        &config,
        &market,
        &model,
        &budget,
        &builder,
        &|line: &str| lines.lock().unwrap().push(line.to_owned()),
    )
    .unwrap();
    let lines = lines.into_inner().unwrap();

    let first = &report.attempts[0];
    assert_eq!(
        first.builds, 2,
        "the broken build was sent back and repaired"
    );
    assert_eq!(first.status, Status::Passed);
    assert!(first.in_sample.is_some() && first.halves.is_some());
    assert!(
        first.out_of_sample.is_some(),
        "survivors are run on the test period at the end"
    );
    assert_eq!(report.attempts[1].status, Status::Rejected);

    assert_eq!(report.funnel.proposed, 2);
    assert_eq!(report.funnel.compiled, 2);
    assert_eq!(report.funnel.passed_critic, 1);
    assert!(report.deflated.is_some());
    // 2 ideas, 3 code answers (one repair), 2 critiques at 1 cent each.
    assert!(
        (report.spent_usd - 0.07).abs() < 1e-9,
        "{}",
        report.spent_usd
    );

    let experiment: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.path().join("experiment.json")).unwrap())
            .unwrap();
    assert_eq!(experiment["placeholder"], false);
    assert_eq!(experiment["funnel"]["passedCritic"], 1);
    assert_eq!(experiment["strategies"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(out.path().join("attempts.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(lines.iter().any(|l| l.contains("build 1")), "{lines:?}");
}

#[test]
fn the_run_stops_cleanly_when_the_budget_runs_out() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let builder = GuestBuilder::new(&root).unwrap();
    let out = tempfile::tempdir().unwrap();
    let start = Timestamp::from_ymd(2024, 1, 1);
    let market = Market {
        instrument: Instrument::new(
            BTC,
            "BTCUSDT",
            "0.01".parse().unwrap(),
            "0.00001".parse().unwrap(),
        ),
        train: Period {
            from: "a".into(),
            to: "b".into(),
            events: bars(start, 500, 3),
        },
        test: Period {
            from: "c".into(),
            to: "d".into(),
            events: bars(start + TimeDelta::from_days(1), 500, 5),
        },
        warmup: Vec::new(),
    };
    // Each call costs 40 cents against a 50 cent cap: the first idea is
    // affordable, the code for it isn't.
    let model = Scripted::new(0.4).answer(
        "hypothesis",
        [json!({"name": "x", "hypothesis": "y", "rules": "z"})],
    );
    let config = LabConfig {
        attempts: 10,
        workers: 3,
        max_repairs: 1,
        initial_cash: Money::from_int(10_000).unwrap(),
        execution: ExecutionModel::realistic(),
        limits: Limits::default(),
        out_dir: out.path().to_owned(),
    };
    let budget = Budget::new(0.5);
    let report = run(&config, &market, &model, &budget, &builder, &|_: &str| {}).unwrap();
    assert!(report.stopped.contains("budget"), "{}", report.stopped);
    assert!(report.spent_usd <= 0.5);
    assert!(out.path().join("experiment.json").exists());
}
