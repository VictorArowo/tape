//! The interface between a strategy and the engine.
//!
//! A strategy never touches the data source or the clock. It receives events
//! one at a time, and everything it can ask about (positions, cash, prices,
//! open orders) reflects only what has happened up to now. That is what
//! makes look-ahead impossible rather than merely discouraged.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tape_core::{
    Fill, Instrument, InstrumentId, MarketEvent, Money, OrderId, OrderRequest, Price, Qty,
    Timestamp,
};

use crate::config::{FeeModel, PPM};
use crate::portfolio::Portfolio;

pub trait Strategy {
    /// Name used in reports.
    fn name(&self) -> String;

    /// Called once before the first event.
    fn on_start(&mut self, _ctx: &mut Context<'_>) {}

    /// Called for every market event, after any fills it caused have been
    /// applied and reported.
    fn on_market(&mut self, ctx: &mut Context<'_>, event: &MarketEvent);

    fn on_fill(&mut self, _ctx: &mut Context<'_>, _fill: &Fill) {}

    fn on_order(&mut self, _ctx: &mut Context<'_>, _update: &OrderUpdate) {}

    /// Called once after the last event. Orders sent here are never filled.
    fn on_finish(&mut self, _ctx: &mut Context<'_>) {}
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    UnknownInstrument,
    /// Quantity isn't positive or isn't a multiple of the lot size.
    InvalidQty,
    /// Price isn't positive or isn't a multiple of the tick size.
    InvalidPrice,
    InsufficientCash,
    InsufficientPosition,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CancelReason {
    /// The strategy asked for it.
    Requested,
    /// An immediate-or-cancel order's unfilled remainder.
    ImmediateOrCancel,
    /// A fill would have spent more cash than the account had.
    InsufficientCash,
    /// A fill would have sold more than the account held.
    InsufficientPosition,
    EndOfData,
}

impl RejectReason {
    /// The reason's code at the WebAssembly boundary, an index into
    /// [`tape_core::abi::REJECT_REASONS`].
    pub fn abi_code(self) -> i32 {
        match self {
            Self::UnknownInstrument => 0,
            Self::InvalidQty => 1,
            Self::InvalidPrice => 2,
            Self::InsufficientCash => 3,
            Self::InsufficientPosition => 4,
        }
    }
}

impl CancelReason {
    /// The reason's code at the WebAssembly boundary, an index into
    /// [`tape_core::abi::CANCEL_REASONS`].
    pub fn abi_code(self) -> i32 {
        match self {
            Self::Requested => 0,
            Self::ImmediateOrCancel => 1,
            Self::InsufficientCash => 2,
            Self::InsufficientPosition => 3,
            Self::EndOfData => 4,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderUpdate {
    /// The order reached the exchange and is working.
    Accepted {
        id: OrderId,
        ts: Timestamp,
    },
    Rejected {
        id: OrderId,
        ts: Timestamp,
        reason: RejectReason,
    },
    /// The order stopped working with `remaining` unfilled.
    Cancelled {
        id: OrderId,
        ts: Timestamp,
        remaining: Qty,
        reason: CancelReason,
    },
    Filled {
        id: OrderId,
        ts: Timestamp,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderState {
    /// Sent, not yet at the exchange.
    InFlight,
    Working,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenOrder {
    pub request: OrderRequest,
    pub remaining: Qty,
    pub state: OrderState,
}

pub(crate) enum Command {
    Submit(OrderId, OrderRequest),
    Cancel(OrderId),
}

/// What a strategy can see and do during a callback.
pub struct Context<'a> {
    pub(crate) now: Timestamp,
    pub(crate) portfolio: &'a Portfolio,
    pub(crate) marks: &'a BTreeMap<InstrumentId, Price>,
    pub(crate) instruments: &'a BTreeMap<InstrumentId, Instrument>,
    pub(crate) open_orders: &'a BTreeMap<OrderId, OpenOrder>,
    pub(crate) fees: &'a FeeModel,
    pub(crate) next_order_id: &'a mut u64,
    pub(crate) outbox: &'a mut Vec<Command>,
    pub(crate) abort: &'a mut Option<String>,
}

impl Context<'_> {
    /// The time of the event being handled.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// Sends an order. It reaches the exchange after the configured latency.
    pub fn submit(&mut self, request: OrderRequest) -> OrderId {
        *self.next_order_id += 1;
        let id = OrderId(*self.next_order_id);
        self.outbox.push(Command::Submit(id, request));
        id
    }

    /// The id the next [`submit`](Self::submit) will return. Ids are
    /// assigned in sequence, so this lets code that can't call `submit`
    /// directly (such as a sandboxed strategy) know its ids in advance.
    pub fn next_order_id(&self) -> OrderId {
        OrderId(*self.next_order_id + 1)
    }

    /// Stops the backtest with an error once this callback returns. For
    /// strategies that can fail, such as sandboxed code that crashes or
    /// runs out of its compute budget.
    pub fn abort(&mut self, reason: impl Into<String>) {
        self.abort.get_or_insert_with(|| reason.into());
    }

    /// Asks the exchange to cancel an order, after the configured latency.
    /// The order may fill before the cancel arrives.
    pub fn cancel(&mut self, id: OrderId) {
        self.outbox.push(Command::Cancel(id));
    }

    pub fn cash(&self) -> Money {
        self.portfolio.cash()
    }

    pub fn position(&self, instrument: InstrumentId) -> Qty {
        self.portfolio.position(instrument)
    }

    /// Cash plus positions at their last traded or closing price.
    pub fn equity(&self) -> Money {
        self.portfolio.equity(self.marks)
    }

    /// The most recent trade price or bar close, if any has been seen.
    pub fn last_price(&self, instrument: InstrumentId) -> Option<Price> {
        self.marks.get(&instrument).copied()
    }

    pub fn instrument(&self, id: InstrumentId) -> Option<&Instrument> {
        self.instruments.get(&id)
    }

    /// Every instrument in the backtest, in id order.
    pub fn instruments(&self) -> impl Iterator<Item = &Instrument> + '_ {
        self.instruments.values()
    }

    /// Orders sent but not yet finished, including ones still in flight.
    pub fn open_orders(&self) -> impl Iterator<Item = (OrderId, &OpenOrder)> + '_ {
        self.open_orders.iter().map(|(id, order)| (*id, order))
    }

    /// The fee schedule orders will be charged.
    pub fn fees(&self) -> &FeeModel {
        self.fees
    }

    /// The largest quantity, in whole lots, that `cash` buys at `price`
    /// after taker fees and a safety margin for slippage (in ppm).
    pub fn affordable_qty(
        &self,
        instrument: InstrumentId,
        price: Price,
        cash: Money,
        margin_ppm: i64,
    ) -> Qty {
        self.instruments.get(&instrument).map_or(Qty::ZERO, |inst| {
            affordable_qty(inst, self.fees, price, cash, margin_ppm)
        })
    }
}

/// The largest quantity, in whole lots of `instrument`, that `cash` buys at
/// `price` after taker fees and a safety margin (in ppm). Shared by
/// [`Context::affordable_qty`] and the WebAssembly sandbox so both size
/// orders identically.
pub fn affordable_qty(
    instrument: &Instrument,
    fees: &FeeModel,
    price: Price,
    cash: Money,
    margin_ppm: i64,
) -> Qty {
    if !price.is_positive() || !cash.is_positive() {
        return Qty::ZERO;
    }
    // Cost per lot including fees and margin, rounded up to be safe.
    let lot_cost = price * instrument.lot_size;
    let factor = PPM + fees.taker_ppm + margin_ppm;
    let lot_cost = lot_cost.mul_ratio(factor, PPM) + Money::from_raw(1);
    let lots = cash.raw() / lot_cost.raw();
    Qty::from_raw(lots * instrument.lot_size.raw())
}
