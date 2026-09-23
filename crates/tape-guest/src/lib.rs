//! Write a tape strategy that runs inside the WebAssembly sandbox.
//!
//! A sandboxed strategy is an ordinary Rust crate built as a `cdylib` for
//! `wasm32-unknown-unknown`. It implements [`Strategy`] using the same
//! `tape-core` types as the engine, and registers itself with
//! [`export_strategy!`]:
//!
//! ```ignore
//! use tape_guest::{Bar, Ctx, Strategy, export_strategy};
//!
//! struct DoNothing;
//!
//! impl Strategy for DoNothing {
//!     fn name(&self) -> String { "do-nothing".into() }
//!     fn on_bar(&mut self, _ctx: &mut Ctx, _bar: &Bar) {}
//! }
//!
//! export_strategy!(DoNothing);
//! ```
//!
//! Inside the sandbox a strategy can see its account and the prices it has
//! been sent, and it can place and cancel orders. It can't read the clock,
//! generate randomness, touch files or open network connections: the host
//! offers no way to, so there is nothing to forbid.

// The WebAssembly boundary is the only place in tape that needs `unsafe`:
// declaring host imports and exporting unmangled functions.
#![allow(unsafe_code)]

pub use tape_core::{
    Bar, Fill, InstrumentId, Liquidity, Money, OrderId, OrderRequest, OrderType, Price, Qty, Side,
    TimeDelta, TimeInForce, Timestamp, Trade, abi,
};

/// A strategy compiled into the sandbox. Mirrors the engine's `Strategy`
/// trait, with bars and trades delivered separately.
pub trait Strategy {
    fn name(&self) -> String;
    fn on_start(&mut self, _ctx: &mut Ctx) {}
    fn on_bar(&mut self, _ctx: &mut Ctx, _bar: &Bar) {}
    fn on_trade(&mut self, _ctx: &mut Ctx, _trade: &Trade) {}
    fn on_fill(&mut self, _ctx: &mut Ctx, _fill: &Fill) {}
    fn on_order(&mut self, _ctx: &mut Ctx, _update: &OrderUpdate) {}
    fn on_finish(&mut self, _ctx: &mut Ctx) {}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrderUpdate {
    Accepted {
        id: OrderId,
        ts: Timestamp,
    },
    Rejected {
        id: OrderId,
        ts: Timestamp,
        reason: &'static str,
    },
    Cancelled {
        id: OrderId,
        ts: Timestamp,
        remaining: Qty,
        reason: &'static str,
    },
    Filled {
        id: OrderId,
        ts: Timestamp,
    },
}

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "tape")]
    unsafe extern "C" {
        pub safe fn now() -> i64;
        pub safe fn cash() -> i64;
        pub safe fn equity() -> i64;
        pub safe fn position(instrument: i32) -> i64;
        pub safe fn last_price(instrument: i32) -> i64;
        pub safe fn tick_size(instrument: i32) -> i64;
        pub safe fn lot_size(instrument: i32) -> i64;
        pub safe fn open_orders() -> i32;
        pub safe fn affordable_qty(instrument: i32, price: i64, cash: i64, margin_ppm: i64) -> i64;
        pub safe fn submit(
            instrument: i32,
            side: i32,
            qty: i64,
            kind: i32,
            price: i64,
            tif: i32,
        ) -> i64;
        pub safe fn cancel(order_id: i64);
        pub safe fn log(ptr: i32, len: i32);
    }
}

/// Outside WebAssembly there is no host to call. These exist so the SDK
/// compiles (and can be documented and type-checked) on any target.
#[cfg(not(target_arch = "wasm32"))]
#[allow(unused_variables)]
mod host {
    fn outside() -> ! {
        panic!("tape-guest strategies only run inside the tape sandbox")
    }
    pub fn now() -> i64 {
        outside()
    }
    pub fn cash() -> i64 {
        outside()
    }
    pub fn equity() -> i64 {
        outside()
    }
    pub fn position(instrument: i32) -> i64 {
        outside()
    }
    pub fn last_price(instrument: i32) -> i64 {
        outside()
    }
    pub fn tick_size(instrument: i32) -> i64 {
        outside()
    }
    pub fn lot_size(instrument: i32) -> i64 {
        outside()
    }
    pub fn open_orders() -> i32 {
        outside()
    }
    pub fn affordable_qty(instrument: i32, price: i64, cash: i64, margin_ppm: i64) -> i64 {
        outside()
    }
    pub fn submit(instrument: i32, side: i32, qty: i64, kind: i32, price: i64, tif: i32) -> i64 {
        outside()
    }
    pub fn cancel(order_id: i64) {
        outside()
    }
    pub fn log(ptr: i32, len: i32) {
        outside()
    }
}

fn instrument_code(id: InstrumentId) -> i32 {
    i32::try_from(id.0).expect("instrument id fits in i32")
}

/// What a sandboxed strategy can see and do during a callback.
pub struct Ctx {
    _private: (),
}

impl Ctx {
    pub fn now(&self) -> Timestamp {
        Timestamp::from_nanos(host::now())
    }

    pub fn cash(&self) -> Money {
        Money::from_raw(host::cash())
    }

    pub fn equity(&self) -> Money {
        Money::from_raw(host::equity())
    }

    pub fn position(&self, instrument: InstrumentId) -> Qty {
        Qty::from_raw(host::position(instrument_code(instrument)))
    }

    pub fn last_price(&self, instrument: InstrumentId) -> Option<Price> {
        let raw = host::last_price(instrument_code(instrument));
        (raw != 0).then(|| Price::from_raw(raw))
    }

    pub fn tick_size(&self, instrument: InstrumentId) -> Option<Price> {
        let raw = host::tick_size(instrument_code(instrument));
        (raw != 0).then(|| Price::from_raw(raw))
    }

    pub fn lot_size(&self, instrument: InstrumentId) -> Option<Qty> {
        let raw = host::lot_size(instrument_code(instrument));
        (raw != 0).then(|| Qty::from_raw(raw))
    }

    /// Orders sent and not yet finished.
    pub fn open_orders(&self) -> usize {
        host::open_orders().max(0) as usize
    }

    /// The largest quantity `cash` buys at `price` after taker fees and a
    /// margin in ppm, computed exactly as the engine does.
    pub fn affordable_qty(
        &self,
        instrument: InstrumentId,
        price: Price,
        cash: Money,
        margin_ppm: i64,
    ) -> Qty {
        Qty::from_raw(host::affordable_qty(
            instrument_code(instrument),
            price.raw(),
            cash.raw(),
            margin_ppm,
        ))
    }

    pub fn submit(&mut self, order: OrderRequest) -> OrderId {
        let (kind, price) = abi::order_type_code(order.order_type);
        let id = host::submit(
            instrument_code(order.instrument),
            abi::side_code(order.side),
            order.qty.raw(),
            kind,
            price,
            abi::tif_code(order.time_in_force),
        );
        OrderId(id as u64)
    }

    pub fn cancel(&mut self, id: OrderId) {
        host::cancel(id.0 as i64);
    }

    /// Writes a line to the run's log. The host caps how much a strategy
    /// can log.
    pub fn log(&self, message: &str) {
        host::log(message.as_ptr() as usize as i32, message.len() as i32);
    }
}

/// Glue called by the functions [`export_strategy!`] generates. Not part of
/// the public API.
#[doc(hidden)]
pub mod __rt {
    use super::*;

    fn ctx() -> Ctx {
        Ctx { _private: () }
    }

    fn side(code: i32) -> Side {
        abi::side_from(code).expect("host sent an invalid side")
    }

    fn instrument(code: i32) -> InstrumentId {
        InstrumentId(u32::try_from(code).expect("host sent an invalid instrument"))
    }

    pub fn on_start(s: &mut dyn Strategy) {
        s.on_start(&mut ctx());
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_bar(
        s: &mut dyn Strategy,
        instrument_id: i32,
        open_ts: i64,
        close_ts: i64,
        open: i64,
        high: i64,
        low: i64,
        close: i64,
        volume: i64,
    ) {
        let bar = Bar {
            open_ts: Timestamp::from_nanos(open_ts),
            close_ts: Timestamp::from_nanos(close_ts),
            instrument: instrument(instrument_id),
            open: Price::from_raw(open),
            high: Price::from_raw(high),
            low: Price::from_raw(low),
            close: Price::from_raw(close),
            volume: Qty::from_raw(volume),
        };
        s.on_bar(&mut ctx(), &bar);
    }

    pub fn on_trade(
        s: &mut dyn Strategy,
        instrument_id: i32,
        ts: i64,
        price: i64,
        qty: i64,
        aggressor: i32,
    ) {
        let trade = Trade {
            ts: Timestamp::from_nanos(ts),
            instrument: instrument(instrument_id),
            price: Price::from_raw(price),
            qty: Qty::from_raw(qty),
            aggressor: side(aggressor),
        };
        s.on_trade(&mut ctx(), &trade);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn on_fill(
        s: &mut dyn Strategy,
        order_id: i64,
        ts: i64,
        instrument_id: i32,
        side_code: i32,
        price: i64,
        qty: i64,
        fee: i64,
        liquidity: i32,
    ) {
        let fill = Fill {
            order_id: OrderId(order_id as u64),
            ts: Timestamp::from_nanos(ts),
            instrument: instrument(instrument_id),
            side: side(side_code),
            price: Price::from_raw(price),
            qty: Qty::from_raw(qty),
            fee: Money::from_raw(fee),
            liquidity: abi::liquidity_from(liquidity).expect("host sent an invalid liquidity"),
        };
        s.on_fill(&mut ctx(), &fill);
    }

    pub fn on_order(
        s: &mut dyn Strategy,
        order_id: i64,
        ts: i64,
        update: i32,
        reason: i32,
        remaining: i64,
    ) {
        let (id, ts) = (OrderId(order_id as u64), Timestamp::from_nanos(ts));
        let reason_text = |table: &[&'static str]| {
            usize::try_from(reason)
                .ok()
                .and_then(|i| table.get(i).copied())
                .expect("host sent an invalid reason")
        };
        let update = match update {
            abi::UPDATE_ACCEPTED => OrderUpdate::Accepted { id, ts },
            abi::UPDATE_REJECTED => OrderUpdate::Rejected {
                id,
                ts,
                reason: reason_text(&abi::REJECT_REASONS),
            },
            abi::UPDATE_CANCELLED => OrderUpdate::Cancelled {
                id,
                ts,
                remaining: Qty::from_raw(remaining),
                reason: reason_text(&abi::CANCEL_REASONS),
            },
            abi::UPDATE_FILLED => OrderUpdate::Filled { id, ts },
            other => panic!("host sent an invalid order update {other}"),
        };
        s.on_order(&mut ctx(), &update);
    }

    pub fn on_finish(s: &mut dyn Strategy) {
        s.on_finish(&mut ctx());
    }
}

/// Registers a strategy and generates the functions the sandbox calls.
/// Use it once, at the top level of a `cdylib` crate.
#[macro_export]
macro_rules! export_strategy {
    ($init:expr) => {
        ::std::thread_local! {
            static __TAPE_STRATEGY: ::core::cell::RefCell<::std::boxed::Box<dyn $crate::Strategy>> =
                ::core::cell::RefCell::new(::std::boxed::Box::new($init));
            static __TAPE_NAME: ::std::string::String =
                __TAPE_STRATEGY.with(|s| s.borrow().name());
        }

        fn __tape_with(f: impl FnOnce(&mut dyn $crate::Strategy)) {
            __TAPE_STRATEGY.with(|s| f(&mut **s.borrow_mut()));
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tape_abi_version() -> i32 {
            $crate::abi::VERSION
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tape_name_ptr() -> i32 {
            __TAPE_NAME.with(|n| n.as_ptr() as usize as i32)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tape_name_len() -> i32 {
            __TAPE_NAME.with(|n| n.len() as i32)
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tape_on_start() {
            __tape_with($crate::__rt::on_start);
        }

        #[unsafe(no_mangle)]
        #[allow(clippy::too_many_arguments)]
        pub extern "C" fn tape_on_bar(
            instrument: i32,
            open_ts: i64,
            close_ts: i64,
            open: i64,
            high: i64,
            low: i64,
            close: i64,
            volume: i64,
        ) {
            __tape_with(|s| {
                $crate::__rt::on_bar(
                    s, instrument, open_ts, close_ts, open, high, low, close, volume,
                )
            });
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tape_on_trade(
            instrument: i32,
            ts: i64,
            price: i64,
            qty: i64,
            aggressor: i32,
        ) {
            __tape_with(|s| $crate::__rt::on_trade(s, instrument, ts, price, qty, aggressor));
        }

        #[unsafe(no_mangle)]
        #[allow(clippy::too_many_arguments)]
        pub extern "C" fn tape_on_fill(
            order_id: i64,
            ts: i64,
            instrument: i32,
            side: i32,
            price: i64,
            qty: i64,
            fee: i64,
            liquidity: i32,
        ) {
            __tape_with(|s| {
                $crate::__rt::on_fill(
                    s, order_id, ts, instrument, side, price, qty, fee, liquidity,
                )
            });
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tape_on_order(
            order_id: i64,
            ts: i64,
            update: i32,
            reason: i32,
            remaining: i64,
        ) {
            __tape_with(|s| $crate::__rt::on_order(s, order_id, ts, update, reason, remaining));
        }

        #[unsafe(no_mangle)]
        pub extern "C" fn tape_on_finish() {
            __tape_with($crate::__rt::on_finish);
        }
    };
}
