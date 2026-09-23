use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_DAYS: i64 = 90;

struct MonthlyRelativeMomentum {
    closes: VecDeque<(i64, i64)>,
    month: Option<(i64, i64)>,
    pending_target: Option<i64>,
}

// Convert days since the Unix epoch to a Gregorian (year, month) pair.
fn year_month(days_since_epoch: i64) -> (i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524
        - day_of_era / 146_096)
        / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era
        - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month)
}

impl MonthlyRelativeMomentum {
    fn new() -> Self {
        Self {
            closes: VecDeque::new(),
            month: None,
            pending_target: None,
        }
    }

    fn rebalance(&mut self, ctx: &mut Ctx, price: tape_guest::Price, target_numerator: i64) {
        if ctx.open_orders() != 0 || !price.is_positive() {
            return;
        }

        // The numerator is 2 for a full allocation and 1 for a half allocation.
        let target_money_raw = (i128::from(ctx.equity().raw())
            * i128::from(target_numerator)
            / 2) as i64;
        let target_qty_raw = (i128::from(target_money_raw) * 100_000_000_i128
            / i128::from(price.raw())) as i64;
        let target_qty = Qty::from_raw(target_qty_raw.max(0));
        let position = ctx.position(INSTRUMENT);

        if target_qty > position {
            let needed = target_qty - position;
            let affordable = ctx.affordable_qty(INSTRUMENT, price, ctx.cash(), 5_000);
            let buy_qty = if needed < affordable { needed } else { affordable };
            if buy_qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, buy_qty));
            }
        } else if position > target_qty {
            let sell_qty = position - target_qty;
            if sell_qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, sell_qty));
            }
        }
    }
}

impl Strategy for MonthlyRelativeMomentum {
    fn name(&self) -> String {
        "Monthly Relative-Momentum Allocation".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let ts = bar.close_ts.as_nanos();

        // A target determined at a month boundary is acted on at the next bar's open.
        if let Some(target) = self.pending_target.take() {
            self.rebalance(ctx, bar.open, target);
            if ctx.open_orders() != 0 {
                self.pending_target = Some(target);
            }
        }

        let day_nanos = TimeDelta::from_days(1).as_nanos();
        if day_nanos <= 0 {
            return;
        }
        let month = year_month(ts.div_euclid(day_nanos));
        let is_new_month = self.month.is_some_and(|previous| previous != month);
        self.month = Some(month);

        let cutoff = ts - LOOKBACK_DAYS * day_nanos;
        while self.closes.len() > 1
            && self.closes.get(1).is_some_and(|sample| sample.0 <= cutoff)
        {
            self.closes.pop_front();
        }

        if is_new_month {
            let reference = self
                .closes
                .iter()
                .rev()
                .find(|(sample_ts, _)| *sample_ts <= cutoff)
                .map(|(_, close)| *close);
            if let Some(reference_close) = reference {
                let target_numerator = if bar.close.raw() > reference_close { 2 } else { 1 };
                self.pending_target = Some(target_numerator);
            }
        }

        self.closes.push_back((ts, bar.close.raw()));
        let retention = LOOKBACK_DAYS * day_nanos + TimeDelta::from_mins(1).as_nanos();
        while self
            .closes
            .front()
            .is_some_and(|(sample_ts, _)| *sample_ts < ts - retention)
        {
            self.closes.pop_front();
        }
    }
}

export_strategy!(MonthlyRelativeMomentum::new());
