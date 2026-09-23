use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Price, Qty, Side, Strategy, TimeDelta,
    export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const LOOKBACK_DAYS: i64 = 90;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct SmallCashReserve {
    history: VecDeque<(i64, i64)>,
    previous_month: Option<i64>,
    target_qty: Option<Qty>,
    last_attempt_day: Option<i64>,
}

impl SmallCashReserve {
    fn month_index(day: i64) -> i64 {
        // Convert an epoch day to a Gregorian month index without sample-specific dates.
        let z = day + 719_468;
        let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = year + if month <= 2 { 1 } else { 0 };
        year * 12 + month
    }

    fn target_for_weight(equity: f64, price: Price, weight: f64) -> Qty {
        let px = price.to_f64();
        if px <= 0.0 {
            return Qty::from_raw(0);
        }
        let raw = equity / px * 100_000_000.0 * weight;
        if !raw.is_finite() || raw <= 0.0 {
            Qty::from_raw(0)
        } else {
            Qty::from_raw(raw.min(i64::MAX as f64) as i64)
        }
    }

    fn try_rebalance(&mut self, ctx: &mut Ctx, price: Price, day: i64) {
        if ctx.open_orders() != 0 || self.last_attempt_day == Some(day) {
            return;
        }
        let Some(target) = self.target_qty else {
            return;
        };
        let position = ctx.position(INSTRUMENT);
        if position == target {
            self.target_qty = None;
            return;
        }

        if position > target {
            let qty = position - target;
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, qty));
                self.last_attempt_day = Some(day);
            }
        } else {
            let needed = target - position;
            let affordable = ctx.affordable_qty(INSTRUMENT, price, ctx.cash(), SIZING_MARGIN_PPM);
            let qty = if affordable < needed { affordable } else { needed };
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
                self.last_attempt_day = Some(day);
            }
        }
    }
}

impl Strategy for SmallCashReserve {
    fn name(&self) -> String {
        "Small Cash Reserve".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_nanos = TimeDelta::from_days(1).as_nanos();
        let day = bar.close_ts.as_nanos() / day_nanos;
        let month = Self::month_index(day);

        // History excludes the current bar, so a boundary decision uses only
        // closes available through the previous month-end.
        if let (Some(prior_month), Some(&(prior_ts, prior_close))) =
            (self.previous_month, self.history.back())
        {
            if month != prior_month {
                let cutoff = prior_ts - LOOKBACK_DAYS * day_nanos;
                let reference = self
                    .history
                    .iter()
                    .rev()
                    .find(|(ts, _)| *ts <= cutoff)
                    .map(|(_, close)| *close);

                if let Some(reference) = reference {
                    let weight = if prior_close > reference { 0.90 } else { 0.80 };
                    self.target_qty = Some(Self::target_for_weight(
                        ctx.equity().to_f64(),
                        Price::from_raw(prior_close),
                        weight,
                    ));
                    self.last_attempt_day = None;
                }
            }
        }

        self.try_rebalance(ctx, bar.close, day);

        self.history
            .push_back((bar.close_ts.as_nanos(), bar.close.raw()));
        let keep_after = bar.close_ts.as_nanos() - 120 * day_nanos;
        while self.history.front().is_some_and(|(ts, _)| *ts < keep_after) {
            self.history.pop_front();
        }
        self.previous_month = Some(month);
    }
}

export_strategy!(SmallCashReserve {
    history: VecDeque::new(),
    previous_month: None,
    target_qty: None,
    last_attempt_day: None,
});
