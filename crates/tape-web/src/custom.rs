//! Strategies written in JavaScript, run by the real engine.
//!
//! The page compiles a visitor's code and gives the module its callbacks
//! as imports. [`JsStrategy`] then works like the WebAssembly sandbox in
//! `tape-wasm`: before each callback it takes a snapshot of what the
//! strategy may read (time, cash, position, price, open orders); during the
//! callback the JavaScript reads that snapshot and queues orders through
//! the functions below; afterwards the queued orders go through the real
//! engine context. The code never touches the engine itself, so it sees
//! exactly what a native strategy would at the same moment, and nothing
//! later.
//!
//! Numbers cross as `f64` dollars and bitcoin, since that's what JavaScript
//! has. On the way in, quantities are rounded down to whole lots and prices
//! to the nearest tick; anything still invalid is rejected by the engine as
//! it would be for any strategy.

use std::cell::RefCell;

use tape_core::{
    Bar, Fill, Instrument, MarketEvent, Money, OrderId, OrderRequest, Price, Qty, abi,
};
use tape_engine::{Context, FeeModel, OrderUpdate, Strategy, affordable_qty};

/// Room left for slippage when sizing an all-in buy, in ppm. The same
/// margin the reference strategies use.
const SIZING_MARGIN_PPM: i64 = 5_000;
/// Orders and cancels allowed in one callback.
const COMMANDS_PER_CALLBACK: usize = 1_000;
/// Log output kept per run.
const LOG_BYTES: usize = 64 << 10;
/// Larger values (in dollars or bitcoin) are refused rather than overflow.
const MAX_VALUE: f64 = 1e10;

/// Why [`submit`] or [`cancel`] refused, as the negative number they return.
pub const NOT_IN_CALLBACK: f64 = -1.0;
pub const BAD_ARGUMENTS: f64 = -2.0;
pub const TOO_MANY_ORDERS: f64 = -3.0;

/// The strategy's own code. Each callback returns the error message if the
/// code threw.
pub trait Callbacks {
    fn on_start(&mut self) -> Result<(), String>;
    fn on_bar(&mut self, bar: &Bar) -> Result<(), String>;
    fn on_fill(&mut self, fill: &Fill) -> Result<(), String>;
    fn on_order(&mut self, update: &OrderUpdate) -> Result<(), String>;
    fn on_finish(&mut self) -> Result<(), String>;
}

/// What the strategy may read during one callback.
struct Snapshot {
    now_ms: f64,
    cash: Money,
    equity: Money,
    position: Qty,
    price: Option<Price>,
    open_orders: u32,
    next_order_id: u64,
    instrument: Instrument,
    fees: FeeModel,
}

enum Queued {
    Submit(OrderRequest),
    Cancel(OrderId),
}

#[derive(Default)]
struct Bridge {
    /// Set only while a callback runs.
    snapshot: Option<Snapshot>,
    outbox: Vec<Queued>,
    log: Vec<String>,
    log_bytes_left: usize,
}

thread_local! {
    static BRIDGE: RefCell<Bridge> = RefCell::new(Bridge::default());
}

fn read<T>(missing: T, f: impl FnOnce(&Snapshot) -> T) -> T {
    BRIDGE.with(|b| b.borrow().snapshot.as_ref().map_or(missing, f))
}

/// The close time of the bar being handled, in milliseconds since 1970.
pub fn now() -> f64 {
    read(f64::NAN, |s| s.now_ms)
}

pub fn cash() -> f64 {
    read(f64::NAN, |s| s.cash.to_f64())
}

/// Cash plus the position at the last price.
pub fn equity() -> f64 {
    read(f64::NAN, |s| s.equity.to_f64())
}

/// Bitcoin held.
pub fn position() -> f64 {
    read(f64::NAN, |s| s.position.to_f64())
}

/// The last bar's close, or NaN before the first bar.
pub fn price() -> f64 {
    read(f64::NAN, |s| s.price.map_or(f64::NAN, Price::to_f64))
}

/// Orders sent and not yet finished, including ones still in flight.
pub fn open_orders() -> u32 {
    read(0, |s| s.open_orders)
}

fn to_raw(value: f64) -> Option<i64> {
    (value.is_finite() && value.abs() <= MAX_VALUE).then(|| (value * 1e8).round() as i64)
}

/// The most bitcoin all the cash buys at `price` (the last price if it
/// isn't positive), after taker fees and room for slippage, in whole lots.
pub fn max_buy(price: f64) -> f64 {
    read(f64::NAN, |s| {
        let price = match to_raw(price) {
            Some(raw) if raw > 0 => Some(round_to(raw, s.instrument.tick_size.raw())),
            _ => s.price,
        };
        price.map_or(0.0, |price| {
            affordable_qty(&s.instrument, &s.fees, price, s.cash, SIZING_MARGIN_PPM).to_f64()
        })
    })
}

fn round_to(raw: i64, tick: i64) -> Price {
    Price::from_raw((raw + tick / 2).div_euclid(tick) * tick)
}

/// Queues an order and returns the id it will have, or a negative reason.
/// `side`, `kind` and `tif` are the codes in [`tape_core::abi`]; `price` is
/// the limit or stop price, ignored for market orders.
pub fn submit(side: i32, kind: i32, qty: f64, price: f64, tif: i32) -> f64 {
    BRIDGE.with(|b| {
        let mut bridge = b.borrow_mut();
        let Bridge {
            snapshot, outbox, ..
        } = &mut *bridge;
        let Some(s) = snapshot.as_ref() else {
            return NOT_IN_CALLBACK;
        };
        let (Some(qty), Some(price)) = (to_raw(qty), to_raw(price)) else {
            return BAD_ARGUMENTS;
        };
        let lot = s.instrument.lot_size.raw();
        let price = match kind {
            abi::ORDER_MARKET => 0,
            _ => round_to(price, s.instrument.tick_size.raw()).raw(),
        };
        let (Some(side), Some(order_type), Some(time_in_force)) = (
            abi::side_from(side),
            abi::order_type_from(kind, price),
            abi::tif_from(tif),
        ) else {
            return BAD_ARGUMENTS;
        };
        if outbox.len() >= COMMANDS_PER_CALLBACK {
            return TOO_MANY_ORDERS;
        }
        // The engine assigns ids in sequence, so the id is known now.
        let earlier = outbox
            .iter()
            .filter(|q| matches!(q, Queued::Submit(_)))
            .count() as u64;
        outbox.push(Queued::Submit(OrderRequest {
            instrument: s.instrument.id,
            side,
            qty: Qty::from_raw(qty - qty % lot),
            order_type,
            time_in_force,
        }));
        (s.next_order_id + earlier) as f64
    })
}

/// Queues a cancel. Returns 0, or a negative reason.
pub fn cancel(id: f64) -> f64 {
    BRIDGE.with(|b| {
        let mut bridge = b.borrow_mut();
        if bridge.snapshot.is_none() {
            return NOT_IN_CALLBACK;
        }
        if !(id.is_finite() && id >= 1.0 && id.fract() == 0.0 && id < 9e15) {
            return BAD_ARGUMENTS;
        }
        if bridge.outbox.len() >= COMMANDS_PER_CALLBACK {
            return TOO_MANY_ORDERS;
        }
        bridge.outbox.push(Queued::Cancel(OrderId(id as u64)));
        0.0
    })
}

/// Keeps a line of the strategy's output, up to the log limit.
pub fn log(line: &str) {
    BRIDGE.with(|b| {
        let mut bridge = b.borrow_mut();
        let mut keep = line.len().min(bridge.log_bytes_left);
        while !line.is_char_boundary(keep) {
            keep -= 1;
        }
        if keep > 0 {
            bridge.log_bytes_left -= keep;
            bridge.log.push(line[..keep].to_owned());
        }
    })
}

/// The lines logged since the last [`JsStrategy`] was created.
pub fn take_log() -> Vec<String> {
    BRIDGE.with(|b| std::mem::take(&mut b.borrow_mut().log))
}

/// A strategy whose decisions are made by [`Callbacks`].
pub struct JsStrategy<C> {
    callbacks: C,
    failed: bool,
}

impl<C: Callbacks> JsStrategy<C> {
    /// Starts a fresh log for this strategy's run.
    pub fn new(callbacks: C) -> Self {
        BRIDGE.with(|b| {
            *b.borrow_mut() = Bridge {
                log_bytes_left: LOG_BYTES,
                ..Bridge::default()
            };
        });
        Self {
            callbacks,
            failed: false,
        }
    }

    fn invoke(
        &mut self,
        ctx: &mut Context<'_>,
        callback: &str,
        call: impl FnOnce(&mut C) -> Result<(), String>,
    ) {
        if self.failed {
            return;
        }
        let Some(instrument) = ctx.instruments().next().cloned() else {
            return;
        };
        let snapshot = Snapshot {
            now_ms: ctx.now().as_nanos() as f64 / 1e6,
            cash: ctx.cash(),
            equity: ctx.equity(),
            position: ctx.position(instrument.id),
            price: ctx.last_price(instrument.id),
            open_orders: u32::try_from(ctx.open_orders().count()).unwrap_or(u32::MAX),
            next_order_id: ctx.next_order_id().0,
            instrument,
            fees: ctx.fees().clone(),
        };
        BRIDGE.with(|b| {
            let mut bridge = b.borrow_mut();
            bridge.snapshot = Some(snapshot);
            bridge.outbox.clear();
        });
        // The bridge isn't borrowed while the strategy runs, since the
        // strategy calls back into it.
        let result = call(&mut self.callbacks);
        let outbox = BRIDGE.with(|b| {
            let mut bridge = b.borrow_mut();
            bridge.snapshot = None;
            std::mem::take(&mut bridge.outbox)
        });
        if let Err(message) = result {
            self.failed = true;
            ctx.abort(format!("{message} (in {callback})"));
            return;
        }
        for queued in outbox {
            match queued {
                Queued::Submit(order) => {
                    ctx.submit(order);
                }
                Queued::Cancel(id) => ctx.cancel(id),
            }
        }
    }
}

impl<C: Callbacks> Strategy for JsStrategy<C> {
    fn name(&self) -> String {
        "custom".into()
    }

    fn on_start(&mut self, ctx: &mut Context<'_>) {
        self.invoke(ctx, "setup", C::on_start);
    }

    fn on_market(&mut self, ctx: &mut Context<'_>, event: &MarketEvent) {
        if let MarketEvent::Bar(bar) = event {
            self.invoke(ctx, "onBar", |c| c.on_bar(bar));
        }
    }

    fn on_fill(&mut self, ctx: &mut Context<'_>, fill: &Fill) {
        self.invoke(ctx, "onFill", |c| c.on_fill(fill));
    }

    fn on_order(&mut self, ctx: &mut Context<'_>, update: &OrderUpdate) {
        self.invoke(ctx, "onOrder", |c| c.on_order(update));
    }

    fn on_finish(&mut self, ctx: &mut Context<'_>) {
        self.invoke(ctx, "onFinish", C::on_finish);
    }
}

/// An order update as the numbers the page's callback takes: order id,
/// time in ms, update code, reason code (or [`abi::NO_REASON`]), and the
/// quantity left unfilled.
pub fn update_args(update: &OrderUpdate) -> (f64, f64, i32, i32, f64) {
    let ms = |ts: tape_core::Timestamp| ts.as_nanos() as f64 / 1e6;
    match *update {
        OrderUpdate::Accepted { id, ts } => (
            id.0 as f64,
            ms(ts),
            abi::UPDATE_ACCEPTED,
            abi::NO_REASON,
            0.0,
        ),
        OrderUpdate::Rejected { id, ts, reason } => (
            id.0 as f64,
            ms(ts),
            abi::UPDATE_REJECTED,
            reason.abi_code(),
            0.0,
        ),
        OrderUpdate::Cancelled {
            id,
            ts,
            remaining,
            reason,
        } => (
            id.0 as f64,
            ms(ts),
            abi::UPDATE_CANCELLED,
            reason.abi_code(),
            remaining.to_f64(),
        ),
        OrderUpdate::Filled { id, ts } => {
            (id.0 as f64, ms(ts), abi::UPDATE_FILLED, abi::NO_REASON, 0.0)
        }
    }
}
