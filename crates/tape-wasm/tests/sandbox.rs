//! What the sandbox promises: faithful results, and hard limits on what
//! untrusted code can do.

use std::convert::Infallible;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use tape_core::{
    Bar, Instrument, InstrumentId, MarketEvent, Money, Price, Qty, TimeDelta, Timestamp,
};
use tape_engine::{Backtest, BacktestConfig, EngineError, ExecutionModel};
use tape_strategies::SmaCross;
use tape_wasm::{Limits, Sandbox, SandboxError};

const BTC: InstrumentId = InstrumentId(1);

fn btc() -> Instrument {
    Instrument::new(
        BTC,
        "BTCUSDT",
        "0.01".parse().unwrap(),
        "0.00001".parse().unwrap(),
    )
}

/// A deterministic random walk of one-minute bars.
fn market(bars: usize) -> Vec<MarketEvent> {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let start = Timestamp::from_ymd(2024, 1, 1);
    let mut price = 4_200_000_000_000_i64; // 42,000.00
    (0..bars)
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

fn backtest(execution: ExecutionModel) -> Backtest {
    let config = BacktestConfig {
        initial_cash: Money::from_int(10_000).unwrap(),
        execution,
        ..BacktestConfig::default()
    };
    Backtest::new(config, [btc()])
}

fn events(bars: usize) -> impl Iterator<Item = Result<MarketEvent, Infallible>> {
    market(bars).into_iter().map(Ok)
}

/// Builds `guests/sma-cross` for wasm32 once per test run, in its own target
/// directory so it doesn't contend with the cargo running these tests.
fn sma_cross_wasm() -> &'static [u8] {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let target_dir = root.join("target/guests");
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "--release",
                "--quiet",
                "--target",
                "wasm32-unknown-unknown",
            ])
            .arg("--manifest-path")
            .arg(root.join("guests/sma-cross/Cargo.toml"))
            .arg("--target-dir")
            .arg(&target_dir)
            .status()
            .expect("cargo runs");
        assert!(status.success(), "building the sma-cross guest failed");
        std::fs::read(target_dir.join("wasm32-unknown-unknown/release/tape_guest_sma_cross.wasm"))
            .expect("guest was built")
    })
}

#[test]
fn a_sandboxed_strategy_matches_its_native_twin_exactly() {
    let sandbox = Sandbox::new(Limits::default());
    for execution in [ExecutionModel::naive(), ExecutionModel::realistic()] {
        let native = backtest(execution.clone())
            .run(&mut SmaCross::new(BTC, 5, 20), events(3_000))
            .unwrap();
        let mut guest = sandbox.load(sma_cross_wasm()).unwrap();
        let sandboxed = backtest(execution).run(&mut guest, events(3_000)).unwrap();

        assert!(
            native.fills.len() > 50,
            "the test should exercise real trading"
        );
        assert_eq!(sandboxed.orders, native.orders);
        assert_eq!(sandboxed.fills, native.fills);
        assert_eq!(sandboxed.equity, native.equity);
        assert_eq!(sandboxed.run_hash, native.run_hash);
        assert_eq!(sandboxed.strategy, "sma-cross(5,20) [wasm]");
        assert!(guest.fuel_used() > 0);
    }
}

/// A minimal module in the text format, with every export the sandbox
/// needs. `on_bar` is the body of `tape_on_bar`; `extra` goes at the top.
fn module(extra: &str, on_bar: &str) -> String {
    format!(
        r#"(module
          {extra}
          (memory (export "memory") 1)
          (func (export "tape_abi_version") (result i32) i32.const 1)
          (func (export "tape_name_ptr") (result i32) i32.const 0)
          (func (export "tape_name_len") (result i32) i32.const 0)
          (func (export "tape_on_start"))
          (func (export "tape_on_bar") (param i32 i64 i64 i64 i64 i64 i64 i64) {on_bar})
          (func (export "tape_on_trade") (param i32 i64 i64 i64 i32))
          (func (export "tape_on_fill") (param i64 i64 i32 i32 i64 i64 i64 i32))
          (func (export "tape_on_order") (param i64 i64 i32 i32 i64))
          (func (export "tape_on_finish")))"#
    )
}

fn run_module(source: &str) -> Result<tape_engine::BacktestResult, EngineError> {
    let mut strategy = Sandbox::new(Limits::default()).load(source).unwrap();
    backtest(ExecutionModel::naive()).run(&mut strategy, events(10))
}

fn aborted_reason(result: Result<tape_engine::BacktestResult, EngineError>) -> String {
    match result {
        Err(EngineError::StrategyAborted { reason, .. }) => reason,
        Err(other) => panic!("expected an abort, got {other}"),
        Ok(_) => panic!("expected an abort, but the run finished"),
    }
}

#[test]
fn a_well_behaved_module_runs() {
    let result = run_module(&module("", "")).unwrap();
    assert_eq!(result.events, 10);
    assert_eq!(result.strategy, "wasm strategy");
}

#[test]
fn an_infinite_loop_runs_out_of_fuel_instead_of_hanging() {
    let reason = aborted_reason(run_module(&module("", "(loop $forever (br $forever))")));
    assert!(reason.contains("ran out of its compute budget"), "{reason}");
    assert!(reason.contains("on_bar"), "{reason}");
}

#[test]
fn memory_is_capped() {
    // Ask for 4 GiB when the limit is 64 MiB.
    let reason = aborted_reason(run_module(&module(
        "",
        "(drop (memory.grow (i32.const 65535)))",
    )));
    assert!(
        reason.contains("went over its memory limit of 64 MiB"),
        "{reason}"
    );
}

#[test]
fn a_trap_ends_the_run_with_the_reason() {
    let reason = aborted_reason(run_module(&module("", "unreachable")));
    assert!(reason.contains("panicked"), "{reason}");
}

#[test]
fn the_clock_the_filesystem_and_the_network_are_not_available() {
    let wasi_clock = r#"(import "wasi_snapshot_preview1" "clock_time_get" (func (param i32 i64 i32) (result i32)))"#;
    let err = Sandbox::new(Limits::default())
        .load(module(wasi_clock, ""))
        .err()
        .unwrap();
    assert!(
        matches!(err, SandboxError::ForbiddenImport(ref name) if name == "wasi_snapshot_preview1::clock_time_get"),
        "{err}"
    );

    let made_up = r#"(import "tape" "wall_clock" (func (result i64)))"#;
    let err = Sandbox::new(Limits::default())
        .load(module(made_up, ""))
        .err()
        .unwrap();
    assert!(matches!(err, SandboxError::ForbiddenImport(_)), "{err}");
}

#[test]
fn modules_must_speak_this_abi() {
    let old = module("", "").replace("i32.const 1)", "i32.const 0)");
    assert!(matches!(
        Sandbox::new(Limits::default()).load(old).err().unwrap(),
        SandboxError::AbiVersion {
            found: 0,
            expected: 1
        }
    ));

    let incomplete = module("", "").replace(r#"(func (export "tape_on_finish"))"#, "");
    assert!(matches!(
        Sandbox::new(Limits::default()).load(incomplete).err().unwrap(),
        SandboxError::MissingExport(ref name) if name == "tape_on_finish"
    ));

    assert!(matches!(
        Sandbox::new(Limits::default())
            .load(b"not wasm")
            .err()
            .unwrap(),
        SandboxError::Invalid(_)
    ));
}

#[test]
fn orders_from_a_module_are_validated_like_any_other() {
    // Submits an order with side code 9 on the first bar.
    let submit =
        r#"(import "tape" "submit" (func $submit (param i32 i32 i64 i32 i64 i32) (result i64)))"#;
    let body = "(drop (call $submit (i32.const 1) (i32.const 9) (i64.const 100000) (i32.const 0) (i64.const 0) (i32.const 1)))";
    let reason = aborted_reason(run_module(&module(submit, body)));
    assert!(reason.contains("invalid side 9"), "{reason}");
}

#[test]
fn a_module_cannot_flood_the_exchange() {
    let submit =
        r#"(import "tape" "submit" (func $submit (param i32 i32 i64 i32 i64 i32) (result i64)))"#;
    let body = r#"(loop $again
        (drop (call $submit (i32.const 1) (i32.const 0) (i64.const 1000) (i32.const 1) (i64.const 100000000) (i32.const 0)))
        (br $again))"#;
    let reason = aborted_reason(run_module(&module(submit, body)));
    assert!(reason.contains("more than 1000 orders"), "{reason}");
}
