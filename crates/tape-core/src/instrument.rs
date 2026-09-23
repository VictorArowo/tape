//! Tradable instruments and their trading rules.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::fixed::{Price, Qty};

/// A compact handle for an instrument, used in every event instead of the
/// symbol string.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct InstrumentId(pub u32);

impl fmt::Display for InstrumentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// An instrument and the increments an exchange accepts for it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    pub id: InstrumentId,
    /// Exchange symbol, e.g. `BTCUSDT`.
    pub symbol: String,
    /// Smallest price increment. Order prices must be a multiple of it.
    pub tick_size: Price,
    /// Smallest quantity increment. Order quantities must be a multiple of it.
    pub lot_size: Qty,
}

impl Instrument {
    pub fn new(
        id: InstrumentId,
        symbol: impl Into<String>,
        tick_size: Price,
        lot_size: Qty,
    ) -> Self {
        assert!(tick_size.is_positive(), "tick size must be positive");
        assert!(lot_size.is_positive(), "lot size must be positive");
        Self {
            id,
            symbol: symbol.into(),
            tick_size,
            lot_size,
        }
    }

    pub fn is_valid_price(&self, price: Price) -> bool {
        price.is_positive() && price.raw() % self.tick_size.raw() == 0
    }

    pub fn is_valid_qty(&self, qty: Qty) -> bool {
        qty.is_positive() && qty.raw() % self.lot_size.raw() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_increments() {
        let btc = Instrument::new(
            InstrumentId(1),
            "BTCUSDT",
            "0.01".parse().unwrap(),
            "0.00001".parse().unwrap(),
        );
        assert!(btc.is_valid_price("64250.51".parse().unwrap()));
        assert!(!btc.is_valid_price("64250.515".parse().unwrap()));
        assert!(!btc.is_valid_price(Price::ZERO));
        assert!(btc.is_valid_qty("0.00123".parse().unwrap()));
        assert!(!btc.is_valid_qty("0.000001".parse().unwrap()));
        assert!(!btc.is_valid_qty("-1".parse().unwrap()));
    }
}
