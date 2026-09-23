//! Orders a strategy can send, and the fills that come back.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fixed::{Money, Price, Qty};
use crate::instrument::InstrumentId;
use crate::market::Side;
use crate::time::Timestamp;

/// Assigned by the engine when an order is submitted. Increases
/// monotonically within a run, so it doubles as submission order.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OrderId(pub u64);

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "order-{}", self.0)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    /// Fill immediately at whatever the market offers.
    Market,
    /// Fill at `price` or better.
    Limit { price: Price },
    /// Becomes a market order once the market trades through `trigger`.
    Stop { trigger: Price },
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    /// Good till cancelled: rests until filled or cancelled.
    #[default]
    Gtc,
    /// Immediate or cancel: fill what's possible on arrival, cancel the rest.
    Ioc,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub instrument: InstrumentId,
    pub side: Side,
    /// Always positive; `side` carries the direction.
    pub qty: Qty,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
}

impl OrderRequest {
    pub fn market(instrument: InstrumentId, side: Side, qty: Qty) -> Self {
        Self {
            instrument,
            side,
            qty,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::Ioc,
        }
    }

    pub fn limit(instrument: InstrumentId, side: Side, qty: Qty, price: Price) -> Self {
        Self {
            instrument,
            side,
            qty,
            order_type: OrderType::Limit { price },
            time_in_force: TimeInForce::Gtc,
        }
    }

    pub fn stop(instrument: InstrumentId, side: Side, qty: Qty, trigger: Price) -> Self {
        Self {
            instrument,
            side,
            qty,
            order_type: OrderType::Stop { trigger },
            time_in_force: TimeInForce::Gtc,
        }
    }

    pub fn with_time_in_force(mut self, time_in_force: TimeInForce) -> Self {
        self.time_in_force = time_in_force;
        self
    }
}

/// Whether a fill added liquidity to the book or took it. Exchanges charge
/// takers more, and the difference often decides whether a strategy is
/// profitable.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Liquidity {
    Maker,
    Taker,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: OrderId,
    pub ts: Timestamp,
    pub instrument: InstrumentId,
    pub side: Side,
    pub price: Price,
    pub qty: Qty,
    /// Fee charged for this fill, in the quote currency. Negative for rebates.
    pub fee: Money,
    pub liquidity: Liquidity,
}

impl Fill {
    /// Price times quantity, before fees.
    pub fn notional(&self) -> Money {
        self.price * self.qty
    }

    /// Change in cash from this fill: buys spend notional, sells receive it,
    /// and fees are always paid.
    pub fn cash_delta(&self) -> Money {
        match self.side {
            Side::Buy => -self.notional() - self.fee,
            Side::Sell => self.notional() - self.fee,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(side: Side) -> Fill {
        Fill {
            order_id: OrderId(1),
            ts: Timestamp::from_ymd(2024, 1, 1),
            instrument: InstrumentId(1),
            side,
            price: "100".parse().unwrap(),
            qty: "2".parse().unwrap(),
            fee: "0.2".parse().unwrap(),
            liquidity: Liquidity::Taker,
        }
    }

    #[test]
    fn cash_moves_the_right_way() {
        assert_eq!(fill(Side::Buy).cash_delta(), "-200.2".parse().unwrap());
        assert_eq!(fill(Side::Sell).cash_delta(), "199.8".parse().unwrap());
    }

    #[test]
    fn constructors_pick_sensible_defaults() {
        let qty: Qty = "1".parse().unwrap();
        assert_eq!(
            OrderRequest::market(InstrumentId(1), Side::Buy, qty).time_in_force,
            TimeInForce::Ioc
        );
        let limit = OrderRequest::limit(InstrumentId(1), Side::Sell, qty, "10".parse().unwrap())
            .with_time_in_force(TimeInForce::Ioc);
        assert_eq!(limit.time_in_force, TimeInForce::Ioc);
    }
}
