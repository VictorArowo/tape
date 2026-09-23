use std::collections::VecDeque;

use tape_guest::{Bar, Ctx, InstrumentId, Money, OrderRequest, Qty, Side, Strategy, TimeDelta, export_strategy};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const SIZING_MARGIN_PPM: i64 = 5_000;
const WEEKEND_DROP_NUM: i64 = 3;
const WEEKEND_DROP_DEN: i64 = 100;

struct WeekendReopeningReversal {
    // Recent completed closes, keyed by their UTC close timestamp in nanoseconds.
    closes: VecDeque<(i64, i64)>,
}

impl WeekendReopeningReversal {
    fn new() -> Self {
        Self { closes: VecDeque::new() }
    }

    fn remember_close(&mut self, ts: i64, price_raw: i64) {
        self.closes.push_back((ts, price_raw));
        let keep_after = ts - TimeDelta::from_days(3).as_nanos();
        while self.closes.front().is_some_and(|(old_ts, _)| *old_ts < keep_after) {
            self.closes.pop_front();
        }
    }

    fn close_at(&self, ts: i64) -> Option<i64> {
        self.closes.iter().find_map(|(stored_ts, price)| (*stored_ts == ts).then_some(*price))
    }

    fn minimum_qty(a: Qty, b: Qty) -> Qty {
        Qty::from_raw(a.raw().min(b.raw()))
    }
}

impl Strategy for WeekendReopeningReversal {
    fn name(&self) -> String {
        "weekend-reopening-reversal".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let now = bar.close_ts.as_nanos();
        self.remember_close(now, bar.close.raw());

        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_of_day = (now / minute_ns) % 1440;
        let day_of_week = (now / day_ns) % 7;

        // At Wednesday 00:00 UTC, submit a market order for execution at the next bar open.
        if day_of_week == 6 && minute_of_day == 0 {
            if ctx.open_orders() == 0 {
                let position = ctx.position(INSTRUMENT);
                if position.is_positive() {
                    ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
                }
            }
            return;
        }

        // Monday is day 4 when Thursday is day 0. The weekend reference is the
        // completed Saturday 00:00 UTC close, exactly two days earlier.
        if day_of_week != 4 || minute_of_day != 0 || ctx.open_orders() != 0 {
            return;
        }
        if !ctx.position(INSTRUMENT).is_zero() {
            return;
        }

        let Some(saturday_close_raw) = self.close_at(now - TimeDelta::from_days(2).as_nanos()) else {
            return;
        };
        if saturday_close_raw <= 0 {
            return;
        }

        // A decline strictly greater than 3% qualifies.
        let monday_raw = bar.close.raw();
        if monday_raw <= 0
            || i128::from(monday_raw) * i128::from(WEEKEND_DROP_DEN)
                >= i128::from(saturday_close_raw) * i128::from(WEEKEND_DROP_DEN - WEEKEND_DROP_NUM)
        {
            return;
        }

        // Limit notional to 25% of equity, then additionally respect available
        // cash (including the SDK's fee-aware affordability calculation).
        let risk_budget = ctx.equity().mul_ratio(1, 4);
        let cash_budget = ctx.cash();
        let budget = Money::from_raw(risk_budget.raw().min(cash_budget.raw()));
        let affordable = ctx.affordable_qty(INSTRUMENT, bar.close, budget, SIZING_MARGIN_PPM);
        let volume_cap = bar.volume.mul_ratio(1, 10);
        let qty = Self::minimum_qty(affordable, volume_cap);
        if qty.is_positive() {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
        }
    }
}

export_strategy!(WeekendReopeningReversal::new());
