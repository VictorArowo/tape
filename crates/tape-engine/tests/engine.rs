//! Behaviour tests for the simulated exchange. Each test names a rule the
//! engine promises to keep.

use std::collections::BTreeMap;
use std::convert::Infallible;

use tape_core::{
    Bar, Fill, Instrument, InstrumentId, Liquidity, MarketEvent, Money, OrderRequest, Price, Qty,
    Side, TimeDelta, TimeInForce, Timestamp, Trade,
};
use tape_engine::{
    Backtest, BacktestConfig, BacktestResult, CancelReason, Context, ExecutionModel, FeeModel,
    OrderStatus, OrderUpdate, RejectReason, Strategy,
};

const BTC: InstrumentId = InstrumentId(1);

fn btc() -> Instrument {
    Instrument::new(BTC, "BTCUSDT", p("0.01"), q("0.001"))
}

fn p(s: &str) -> Price {
    s.parse().unwrap()
}

fn q(s: &str) -> Qty {
    s.parse().unwrap()
}

fn m(s: &str) -> Money {
    s.parse().unwrap()
}

fn t0() -> Timestamp {
    Timestamp::from_ymd(2024, 1, 1)
}

/// One-minute bars from (open, high, low, close, volume).
fn bars(rows: &[(&str, &str, &str, &str, &str)]) -> Vec<MarketEvent> {
    rows.iter()
        .enumerate()
        .map(|(i, (o, h, l, c, v))| {
            let open_ts = t0() + TimeDelta::from_mins(i as i64);
            MarketEvent::Bar(Bar {
                open_ts,
                close_ts: open_ts + TimeDelta::from_mins(1),
                instrument: BTC,
                open: p(o),
                high: p(h),
                low: p(l),
                close: p(c),
                volume: q(v),
            })
        })
        .collect()
}

fn trade(offset_ms: i64, price: &str, qty: &str, aggressor: Side) -> MarketEvent {
    MarketEvent::Trade(Trade {
        ts: t0() + TimeDelta::from_millis(offset_ms),
        instrument: BTC,
        price: p(price),
        qty: q(qty),
        aggressor,
    })
}

fn config(execution: ExecutionModel) -> BacktestConfig {
    BacktestConfig {
        initial_cash: m("10000"),
        execution,
        ..BacktestConfig::default()
    }
}

/// Sends scripted orders on given events (and cancels), and records what
/// comes back.
#[derive(Default)]
struct Script {
    orders: BTreeMap<usize, Vec<OrderRequest>>,
    cancel_on: BTreeMap<usize, usize>,
    seen: usize,
    sent: Vec<tape_core::OrderId>,
    fills: Vec<Fill>,
    updates: Vec<OrderUpdate>,
}

impl Script {
    fn on(mut self, event_index: usize, order: OrderRequest) -> Self {
        self.orders.entry(event_index).or_default().push(order);
        self
    }

    /// Cancel the n-th sent order when event `event_index` arrives.
    fn cancel(mut self, event_index: usize, nth_order: usize) -> Self {
        self.cancel_on.insert(event_index, nth_order);
        self
    }
}

impl Strategy for Script {
    fn name(&self) -> String {
        "script".into()
    }

    fn on_market(&mut self, ctx: &mut Context<'_>, _event: &MarketEvent) {
        if let Some(orders) = self.orders.remove(&self.seen) {
            for order in orders {
                self.sent.push(ctx.submit(order));
            }
        }
        if let Some(nth) = self.cancel_on.remove(&self.seen) {
            ctx.cancel(self.sent[nth]);
        }
        self.seen += 1;
    }

    fn on_fill(&mut self, _ctx: &mut Context<'_>, fill: &Fill) {
        self.fills.push(fill.clone());
    }

    fn on_order(&mut self, _ctx: &mut Context<'_>, update: &OrderUpdate) {
        self.updates.push(update.clone());
    }
}

fn run(config: BacktestConfig, events: Vec<MarketEvent>, script: &mut Script) -> BacktestResult {
    Backtest::new(config, [btc()])
        .run(script, events.into_iter().map(Ok::<_, Infallible>))
        .unwrap()
}

fn buy(qty: &str) -> OrderRequest {
    OrderRequest::market(BTC, Side::Buy, q(qty))
}

#[test]
fn a_decision_at_a_bar_close_fills_at_the_next_bar_open() {
    let data = bars(&[
        ("100", "101", "99", "100.5", "50"),
        ("103", "104", "102", "103.5", "50"),
    ]);
    let mut s = Script::default().on(0, buy("1"));
    let result = run(config(ExecutionModel::naive()), data, &mut s);
    assert_eq!(s.fills.len(), 1);
    // Not the 100.5 close the strategy saw: that price is already history.
    assert_eq!(s.fills[0].price, p("103"));
    assert_eq!(result.final_cash, m("9897"));
}

#[test]
fn latency_delays_arrival() {
    let data = vec![
        trade(0, "100", "1", Side::Buy),
        trade(50, "101", "1", Side::Buy),
        trade(150, "102", "1", Side::Buy),
    ];
    let mut execution = ExecutionModel::naive();
    execution.latency = TimeDelta::from_millis(100);
    let mut s = Script::default().on(0, buy("1"));
    run(config(execution), data, &mut s);
    assert_eq!(s.fills[0].price, p("102"));
    assert_eq!(s.fills[0].ts, t0() + TimeDelta::from_millis(150));
}

#[test]
fn an_order_never_fills_against_a_trade_at_the_instant_it_was_sent() {
    let data = vec![
        trade(0, "100", "1", Side::Buy),
        trade(0, "99", "1", Side::Sell),
        trade(1, "105", "1", Side::Buy),
    ];
    let mut s = Script::default().on(0, buy("1"));
    run(config(ExecutionModel::naive()), data, &mut s);
    assert_eq!(s.fills[0].price, p("105"));
}

#[test]
fn slippage_and_fees_are_charged_on_taker_fills() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100", "100", "100", "100", "50"),
    ]);
    let mut execution = ExecutionModel::naive();
    execution.slippage_ppm = 1_000;
    execution.fees = FeeModel::BINANCE_SPOT;
    let mut s = Script::default().on(0, buy("1"));
    let result = run(config(execution), data, &mut s);
    let fill = &s.fills[0];
    assert_eq!(
        (fill.price, fill.fee, fill.liquidity),
        (p("100.1"), m("0.1001"), Liquidity::Taker)
    );
    assert_eq!(result.final_cash, m("9899.7999"));
    assert_eq!(result.fees_paid, m("0.1001"));
}

#[test]
fn slipped_prices_round_against_the_trader() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100.03", "101", "99", "100", "50"),
        ("100.03", "101", "99", "100", "50"),
    ]);
    let mut execution = ExecutionModel::naive();
    execution.slippage_ppm = 100; // 0.01%: 100.03 -> 100.040003, not on a 0.01 tick
    let mut s = Script::default()
        .on(0, buy("1"))
        .on(1, OrderRequest::market(BTC, Side::Sell, q("1")));
    run(config(execution), data, &mut s);
    assert_eq!(s.fills[0].price, p("100.05"), "buys round up");
    assert_eq!(s.fills[1].price, p("100.01"), "sells round down");
}

#[test]
fn resting_limits_need_the_market_to_trade_through() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100", "100", "99", "99.5", "50"),  // touches 99
        ("99.5", "100", "98.5", "99", "50"), // trades through 99
    ]);
    let order = OrderRequest::limit(BTC, Side::Buy, q("1"), p("99"));

    let mut strict = Script::default().on(0, order.clone());
    run(
        config(ExecutionModel::realistic()),
        data.clone(),
        &mut strict,
    );
    assert_eq!(strict.fills.len(), 1);
    assert_eq!(strict.fills[0].ts, t0() + TimeDelta::from_mins(3));
    assert_eq!(strict.fills[0].price, p("99"));
    assert_eq!(strict.fills[0].liquidity, Liquidity::Maker);

    let mut lenient = Script::default().on(0, order);
    run(config(ExecutionModel::naive()), data, &mut lenient);
    assert_eq!(lenient.fills[0].ts, t0() + TimeDelta::from_mins(2));
}

#[test]
fn resting_limits_fill_only_against_opposite_aggressors_at_their_price() {
    let data = vec![
        trade(0, "100", "1", Side::Buy),
        trade(10, "99", "0.5", Side::Buy), // buyer-initiated at 99: our bid isn't hit
        trade(20, "99", "0.25", Side::Sell), // seller hits bids at 99
        trade(30, "98", "5", Side::Sell),  // through: fills the rest
    ];
    let mut s = Script::default().on(0, OrderRequest::limit(BTC, Side::Buy, q("1"), p("99")));
    run(config(ExecutionModel::naive()), data, &mut s);
    let fills: Vec<(Qty, i64)> = s
        .fills
        .iter()
        .map(|f| (f.qty, (f.ts - t0()).as_nanos() / 1_000_000))
        .collect();
    // Capped by the size of the trade that hit it, then the remainder.
    assert_eq!(fills, vec![(q("0.25"), 20), (q("0.75"), 30)]);
    assert!(s.fills.iter().all(|f| f.price == p("99")));
}

#[test]
fn marketable_limits_never_fill_worse_than_their_limit() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100", "100", "100", "100", "50"),
    ]);
    let mut execution = ExecutionModel::naive();
    execution.slippage_ppm = 1_000; // would make the fill 100.10
    let mut s = Script::default().on(0, OrderRequest::limit(BTC, Side::Buy, q("1"), p("100.05")));
    run(config(execution), data, &mut s);
    assert_eq!(s.fills[0].price, p("100.05"));
}

#[test]
fn fills_are_capped_by_bar_volume() {
    let data = bars(&[
        ("100", "100", "100", "100", "10"),
        ("100", "100", "100", "100", "10"),
        ("100", "100", "100", "100", "10"),
        ("100", "100", "100", "100", "10"),
    ]);
    let mut execution = ExecutionModel::naive();
    execution.max_bar_participation_ppm = Some(100_000); // 10% of 10 = 1 per bar
    let gtc = buy("2.5").with_time_in_force(TimeInForce::Gtc);
    let mut s = Script::default().on(0, gtc);
    run(config(execution), data, &mut s);
    let qtys: Vec<Qty> = s.fills.iter().map(|f| f.qty).collect();
    assert_eq!(qtys, vec![q("1"), q("1"), q("0.5")]);
}

#[test]
fn immediate_or_cancel_drops_what_cannot_fill_at_once() {
    let data = bars(&[
        ("100", "100", "100", "100", "10"),
        ("100", "100", "100", "100", "10"),
        ("100", "100", "100", "100", "10"),
    ]);
    let mut execution = ExecutionModel::naive();
    execution.max_bar_participation_ppm = Some(100_000);
    let mut s = Script::default().on(0, buy("3"));
    let result = run(config(execution), data, &mut s);
    assert_eq!(s.fills.len(), 1);
    assert!(s.updates.iter().any(|u| matches!(
        u,
        OrderUpdate::Cancelled { remaining, reason: CancelReason::ImmediateOrCancel, .. } if *remaining == q("2")
    )));
    assert_eq!(result.orders[0].filled, q("1"));
}

#[test]
fn stops_that_gap_fill_at_the_open_not_the_trigger() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100", "100", "100", "100", "50"),
        ("90", "91", "89", "90", "50"),
    ]);
    let mut s = Script::default()
        .on(0, buy("1"))
        .on(1, OrderRequest::stop(BTC, Side::Sell, q("1"), p("95")));
    run(config(ExecutionModel::naive()), data, &mut s);
    assert_eq!(s.fills[1].price, p("90"));
}

#[test]
fn orders_the_account_cannot_afford_are_rejected() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100", "100", "100", "100", "50"),
    ]);
    let mut s = Script::default().on(0, buy("101"));
    run(config(ExecutionModel::naive()), data, &mut s);
    assert!(s.fills.is_empty());
    assert!(matches!(
        s.updates[0],
        OrderUpdate::Rejected {
            reason: RejectReason::InsufficientCash,
            ..
        }
    ));
}

#[test]
fn a_fill_that_would_overdraw_cash_cancels_the_order() {
    let data = bars(&[
        ("100", "100", "100", "100", "500"),
        ("120", "120", "120", "120", "500"), // gaps up before the order fills
    ]);
    // Affordable at the last price (90 x 100 = 9,000), not at the fill (10,800).
    let mut s = Script::default().on(0, buy("90"));
    let result = run(config(ExecutionModel::naive()), data, &mut s);
    assert!(result.fills.is_empty());
    assert!(s.updates.iter().any(|u| matches!(
        u,
        OrderUpdate::Cancelled {
            reason: CancelReason::InsufficientCash,
            ..
        }
    )));
    assert_eq!(result.final_cash, m("10000"));
}

#[test]
fn spot_accounts_cannot_sell_what_they_do_not_hold() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100", "100", "100", "100", "50"),
    ]);
    let mut s = Script::default().on(0, OrderRequest::market(BTC, Side::Sell, q("1")));
    run(config(ExecutionModel::naive()), data, &mut s);
    assert!(matches!(
        s.updates[0],
        OrderUpdate::Rejected {
            reason: RejectReason::InsufficientPosition,
            ..
        }
    ));
}

#[test]
fn invalid_sizes_and_prices_are_rejected() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100", "100", "100", "100", "50"),
    ]);
    let mut s = Script::default()
        .on(0, buy("0.0005"))
        .on(0, OrderRequest::limit(BTC, Side::Buy, q("1"), p("99.995")));
    run(config(ExecutionModel::naive()), data, &mut s);
    let reasons: Vec<_> = s
        .updates
        .iter()
        .filter_map(|u| match u {
            OrderUpdate::Rejected { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        vec![RejectReason::InvalidQty, RejectReason::InvalidPrice]
    );
}

#[test]
fn a_cancel_that_arrives_after_the_fill_is_too_late() {
    let data = vec![
        trade(0, "100", "1", Side::Buy),
        trade(10, "99", "1", Side::Buy),
        trade(60, "98", "5", Side::Sell),
        trade(200, "97", "5", Side::Sell),
    ];
    let mut execution = ExecutionModel::naive();
    execution.latency = TimeDelta::from_millis(50);
    let mut s = Script::default()
        .on(0, OrderRequest::limit(BTC, Side::Buy, q("1"), p("99")))
        .cancel(1, 0); // sent at 10ms, arrives at 60ms, after the 60ms fill
    let result = run(config(execution), data, &mut s);
    assert_eq!(s.fills.len(), 1);
    assert_eq!(result.orders[0].status, OrderStatus::Filled);
}

#[test]
fn open_orders_are_cancelled_when_data_ends() {
    let data = bars(&[
        ("100", "100", "100", "100", "50"),
        ("100", "100", "100", "100", "50"),
    ]);
    let mut s = Script::default().on(0, OrderRequest::limit(BTC, Side::Buy, q("1"), p("50")));
    let result = run(config(ExecutionModel::naive()), data, &mut s);
    assert_eq!(
        result.orders[0].status,
        OrderStatus::Cancelled(CancelReason::EndOfData)
    );
}

#[test]
fn identical_inputs_give_identical_runs() {
    let data = bars(&[
        ("100", "101", "99", "100", "50"),
        ("100", "102", "98", "101", "50"),
        ("101", "103", "100", "102", "50"),
    ]);
    let script = || {
        Script::default().on(0, buy("1")).on(
            1,
            OrderRequest::limit(BTC, Side::Sell, q("0.5"), p("102.5")),
        )
    };
    let a = run(
        config(ExecutionModel::realistic()),
        data.clone(),
        &mut script(),
    );
    let b = run(
        config(ExecutionModel::realistic()),
        data.clone(),
        &mut script(),
    );
    assert_eq!(a, b);
    let c = run(config(ExecutionModel::naive()), data, &mut script());
    assert_ne!(a.run_hash, c.run_hash);
}

#[test]
fn equity_is_sampled_on_the_configured_interval() {
    let rows: Vec<_> = (0..10).map(|_| ("100", "100", "100", "100", "1")).collect();
    let mut cfg = config(ExecutionModel::naive());
    cfg.equity_interval = TimeDelta::from_mins(3);
    let result = run(cfg, bars(&rows), &mut Script::default());
    let minutes: Vec<i64> = result
        .equity
        .iter()
        .map(|point| (point.ts - t0()).as_nanos() / 60_000_000_000)
        .collect();
    assert_eq!(minutes, vec![1, 3, 6, 9, 10]);
}

mod properties {
    use super::*;
    // proptest also has a trait named `Strategy` (a value generator);
    // import it anonymously so `prop_map` works without shadowing ours.
    use proptest::prelude::{ProptestConfig, any, prop, prop_assert, prop_assert_eq, proptest};
    use proptest::strategy::Strategy as _;

    /// A strategy driven by a list of random actions.
    struct Random {
        actions: Vec<(u8, u16, i16)>,
        step: usize,
    }

    impl Strategy for Random {
        fn name(&self) -> String {
            "random".into()
        }

        fn on_market(&mut self, ctx: &mut Context<'_>, _event: &MarketEvent) {
            let Some(&(kind, size, offset)) = self.actions.get(self.step) else {
                return;
            };
            self.step += 1;
            let qty = Qty::from_raw(i64::from(size % 2_000 + 1) * 100_000); // 0.001 .. 2.0
            let mark = ctx.last_price(BTC).unwrap_or(p("100"));
            let price =
                Price::from_raw((mark.raw() + i64::from(offset) * 1_000_000).max(1_000_000));
            let tif = if kind & 8 == 0 {
                TimeInForce::Gtc
            } else {
                TimeInForce::Ioc
            };
            let order = match kind % 6 {
                0 => OrderRequest::market(BTC, Side::Buy, qty),
                1 => OrderRequest::market(BTC, Side::Sell, qty),
                2 => OrderRequest::limit(BTC, Side::Buy, qty, price).with_time_in_force(tif),
                3 => OrderRequest::limit(BTC, Side::Sell, qty, price).with_time_in_force(tif),
                4 => OrderRequest::stop(BTC, Side::Sell, qty, price),
                _ => {
                    let oldest = ctx.open_orders().next().map(|(id, _)| id);
                    if let Some(id) = oldest {
                        ctx.cancel(id);
                    }
                    return;
                }
            };
            ctx.submit(order);
        }
    }

    fn market() -> impl proptest::strategy::Strategy<Value = Vec<MarketEvent>> + Clone {
        prop::collection::vec((-300i64..300, 0i64..200, 0i64..200, 1i64..5_000), 2..120).prop_map(
            |moves| {
                let mut price = 10_000_000_000i64; // 100.00
                moves
                    .into_iter()
                    .enumerate()
                    .map(|(i, (drift, up, down, volume))| {
                        let open = price;
                        let close = (open + drift * 1_000_000).max(1_000_000);
                        let high = open.max(close) + up * 1_000_000;
                        let low = (open.min(close) - down * 1_000_000).max(1_000_000);
                        price = close;
                        let open_ts = t0() + TimeDelta::from_mins(i as i64);
                        MarketEvent::Bar(Bar {
                            open_ts,
                            close_ts: open_ts + TimeDelta::from_mins(1),
                            instrument: BTC,
                            open: Price::from_raw(open),
                            high: Price::from_raw(high),
                            low: Price::from_raw(low),
                            close: Price::from_raw(close),
                            volume: Qty::from_raw(volume * 100_000),
                        })
                    })
                    .collect()
            },
        )
    }

    fn execution() -> impl proptest::strategy::Strategy<Value = ExecutionModel> {
        (
            0i64..3,
            0i64..2_000,
            prop::option::of(1_000i64..500_000),
            any::<bool>(),
        )
            .prop_map(
                |(latency_mins, slippage, participation, through)| ExecutionModel {
                    latency: TimeDelta::from_secs(latency_mins * 40),
                    fees: FeeModel::BINANCE_SPOT,
                    slippage_ppm: slippage,
                    max_bar_participation_ppm: participation,
                    require_trade_through: through,
                },
            )
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn accounting_always_balances(
            events in market(),
            execution in execution(),
            actions in prop::collection::vec((any::<u8>(), any::<u16>(), -5i16..5), 0..120),
        ) {
            let cfg = config(execution);
            let mut strategy = Random { actions, step: 0 };
            let result = Backtest::new(cfg.clone(), [btc()])
                .run(&mut strategy, events.clone().into_iter().map(Ok::<_, Infallible>))
                .unwrap();

            // Cash is exactly the starting cash plus every fill's cash movement.
            let cash = result.fills.iter().fold(cfg.initial_cash, |cash, f| cash + f.cash_delta());
            prop_assert_eq!(cash, result.final_cash);

            // A spot account never overdraws or goes short, at any point.
            let (mut running_cash, mut position) = (cfg.initial_cash, Qty::ZERO);
            for fill in &result.fills {
                running_cash += fill.cash_delta();
                position += if fill.side == Side::Buy { fill.qty } else { -fill.qty };
                prop_assert!(!running_cash.is_negative(), "cash went negative");
                prop_assert!(!position.is_negative(), "position went short");
            }

            let instrument = btc();
            for fill in &result.fills {
                let order = &result.orders[(fill.order_id.0 - 1) as usize];
                prop_assert_eq!(order.id, fill.order_id);
                // No fill happens at or before the moment the order was sent.
                prop_assert!(fill.ts > order.submitted_at);
                prop_assert!(fill.ts >= order.arrived_at.unwrap());
                prop_assert!(instrument.is_valid_qty(fill.qty));
                prop_assert!(instrument.is_valid_price(fill.price));
            }

            // Orders never fill more than they asked for.
            for order in &result.orders {
                prop_assert!(order.filled <= order.request.qty);
                let filled: Qty = result.fills.iter().filter(|f| f.order_id == order.id).map(|f| f.qty).sum();
                prop_assert_eq!(filled, order.filled);
                if order.status == OrderStatus::Filled {
                    prop_assert_eq!(order.filled, order.request.qty);
                }
            }

            // Final equity is cash plus the position at the last close.
            if let Some(MarketEvent::Bar(last)) = events.last() {
                prop_assert_eq!(result.final_equity, result.final_cash + last.close * position);
            }
        }

        #[test]
        fn runs_are_deterministic(
            events in market(),
            execution in execution(),
            actions in prop::collection::vec((any::<u8>(), any::<u16>(), -5i16..5), 0..60),
        ) {
            let cfg = config(execution);
            let run = || {
                Backtest::new(cfg.clone(), [btc()])
                    .run(&mut Random { actions: actions.clone(), step: 0 }, events.clone().into_iter().map(Ok::<_, Infallible>))
                    .unwrap()
            };
            prop_assert_eq!(run(), run());
        }
    }
}
