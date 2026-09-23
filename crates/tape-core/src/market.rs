//! Market data events: what a strategy can observe about the market.

use serde::{Deserialize, Serialize};

use crate::fixed::{Price, Qty};
use crate::instrument::InstrumentId;
use crate::time::Timestamp;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }

    /// +1 for buys, -1 for sells: the direction a fill moves a position.
    pub fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
        }
    }
}

/// A single print on the exchange's tape.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trade {
    pub ts: Timestamp,
    pub instrument: InstrumentId,
    pub price: Price,
    pub qty: Qty,
    /// The side that crossed the spread. A `Buy` aggressor lifted the ask,
    /// so resting sell orders at this price were being filled.
    pub aggressor: Side,
}

/// An OHLCV bar covering `[open_ts, close_ts)`.
///
/// A bar only exists once its interval is over, so its event time is
/// `close_ts`. Stamping bars with their open time is a classic look-ahead
/// bug: the strategy would see the bar's high, low and close at the moment
/// the interval started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bar {
    pub open_ts: Timestamp,
    pub close_ts: Timestamp,
    pub instrument: InstrumentId,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: Qty,
}

impl Bar {
    /// Whether the OHLC values are internally consistent.
    pub fn is_consistent(&self) -> bool {
        self.open_ts < self.close_ts
            && self.low <= self.open.min(self.close)
            && self.high >= self.open.max(self.close)
            && !self.volume.is_negative()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MarketEvent {
    Trade(Trade),
    Bar(Bar),
}

impl MarketEvent {
    /// When the event became observable.
    pub fn ts(&self) -> Timestamp {
        match self {
            MarketEvent::Trade(trade) => trade.ts,
            MarketEvent::Bar(bar) => bar.close_ts,
        }
    }

    pub fn instrument(&self) -> InstrumentId {
        match self {
            MarketEvent::Trade(trade) => trade.instrument,
            MarketEvent::Bar(bar) => bar.instrument,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::TimeDelta;

    fn bar() -> Bar {
        let open_ts = Timestamp::from_ymd(2024, 1, 1);
        Bar {
            open_ts,
            close_ts: open_ts + TimeDelta::from_mins(1),
            instrument: InstrumentId(1),
            open: "100".parse().unwrap(),
            high: "105".parse().unwrap(),
            low: "99".parse().unwrap(),
            close: "104".parse().unwrap(),
            volume: "12.5".parse().unwrap(),
        }
    }

    #[test]
    fn bars_are_observable_at_close() {
        let bar = bar();
        let close = bar.close_ts;
        assert_eq!(MarketEvent::Bar(bar).ts(), close);
    }

    #[test]
    fn detects_inconsistent_bars() {
        assert!(bar().is_consistent());
        assert!(
            !Bar {
                high: "103".parse().unwrap(),
                ..bar()
            }
            .is_consistent()
        );
        assert!(
            !Bar {
                low: "101".parse().unwrap(),
                ..bar()
            }
            .is_consistent()
        );
        assert!(
            !Bar {
                close_ts: bar().open_ts,
                ..bar()
            }
            .is_consistent()
        );
    }

    #[test]
    fn side_helpers() {
        assert_eq!(Side::Buy.opposite(), Side::Sell);
        assert_eq!(Side::Sell.sign(), -1);
    }
}
