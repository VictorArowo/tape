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

    /// Orders sent but not yet finished, including ones still in flight.
    pub fn open_orders(&self) -> impl Iterator<Item = (OrderId, &OpenOrder)> + '_ {
        self.open_orders.iter().map(|(id, order)| (*id, order))
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
        let Some(inst) = self.instruments.get(&instrument) else {
            return Qty::ZERO;
        };
        if !price.is_positive() || !cash.is_positive() {
            return Qty::ZERO;
        }
        // cost per lot, including fees and margin, rounded up to be safe
        let lot_cost = price * inst.lot_size;
        let factor = PPM + self.fees.taker_ppm + margin_ppm;
        let lot_cost = lot_cost.mul_ratio(factor, PPM) + Money::from_raw(1);
        let lots = cash.raw() / lot_cost.raw();
        Qty::from_raw(lots * inst.lot_size.raw())
    }
}
