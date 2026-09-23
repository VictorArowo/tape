//! Cash and positions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tape_core::{Fill, InstrumentId, Money, Price, Qty};

/// Everything the account holds. Uses ordered maps so iteration order, and
/// therefore every derived number, is the same on every run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Portfolio {
    cash: Money,
    positions: BTreeMap<InstrumentId, Qty>,
    fees_paid: Money,
}

impl Portfolio {
    pub fn new(cash: Money) -> Self {
        Self {
            cash,
            positions: BTreeMap::new(),
            fees_paid: Money::ZERO,
        }
    }

    pub fn cash(&self) -> Money {
        self.cash
    }

    pub fn fees_paid(&self) -> Money {
        self.fees_paid
    }

    pub fn position(&self, instrument: InstrumentId) -> Qty {
        self.positions
            .get(&instrument)
            .copied()
            .unwrap_or(Qty::ZERO)
    }

    /// Non-zero positions, in instrument order.
    pub fn positions(&self) -> impl Iterator<Item = (InstrumentId, Qty)> + '_ {
        self.positions
            .iter()
            .filter(|(_, qty)| !qty.is_zero())
            .map(|(id, qty)| (*id, *qty))
    }

    pub fn apply(&mut self, fill: &Fill) {
        self.cash += fill.cash_delta();
        self.fees_paid += fill.fee;
        let position = self.positions.entry(fill.instrument).or_default();
        *position = match fill.side {
            tape_core::Side::Buy => *position + fill.qty,
            tape_core::Side::Sell => *position - fill.qty,
        };
    }

    /// Cash plus every position valued at its mark price. Positions with no
    /// mark yet are valued at zero.
    pub fn equity(&self, marks: &BTreeMap<InstrumentId, Price>) -> Money {
        self.positions().fold(self.cash, |equity, (id, qty)| {
            equity + marks.get(&id).map_or(Money::ZERO, |mark| *mark * qty)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tape_core::{Liquidity, OrderId, Side, Timestamp};

    fn fill(side: Side, price: &str, qty: &str, fee: &str) -> Fill {
        Fill {
            order_id: OrderId(1),
            ts: Timestamp::EPOCH,
            instrument: InstrumentId(1),
            side,
            price: price.parse().unwrap(),
            qty: qty.parse().unwrap(),
            fee: fee.parse().unwrap(),
            liquidity: Liquidity::Taker,
        }
    }

    #[test]
    fn tracks_cash_positions_and_equity() {
        let mut p = Portfolio::new("1000".parse().unwrap());
        p.apply(&fill(Side::Buy, "100", "3", "0.3"));
        p.apply(&fill(Side::Sell, "110", "1", "0.11"));
        assert_eq!(p.cash(), "809.59".parse().unwrap());
        assert_eq!(p.position(InstrumentId(1)), "2".parse().unwrap());
        assert_eq!(p.fees_paid(), "0.41".parse().unwrap());
        let marks = BTreeMap::from([(InstrumentId(1), "120".parse().unwrap())]);
        assert_eq!(p.equity(&marks), "1049.59".parse().unwrap());
        p.apply(&fill(Side::Sell, "120", "2", "0"));
        assert_eq!(p.positions().count(), 0);
    }
}
