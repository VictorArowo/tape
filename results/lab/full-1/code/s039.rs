use std::collections::VecDeque;

use tape_guest::{
    Bar, Ctx, InstrumentId, OrderRequest, Side, Strategy, TimeDelta, export_strategy,
};

const INSTRUMENT: InstrumentId = InstrumentId(1);
const TRAILING_DAYS: i64 = 30;
const SIZING_MARGIN_PPM: i64 = 5_000;

struct PeakDrawdownBrake {
    active_day: Option<i64>,
    active_close_raw: Option<i64>,
    last_finalized_day: Option<i64>,
    daily_closes: VecDeque<(i64, i64)>,
    consecutive_near_high: usize,
}

impl PeakDrawdownBrake {
    fn finalize_day(&mut self, day: i64, close_raw: i64, ctx: &mut Ctx, sizing_price: tape_guest::Price) {
        if self.last_finalized_day == Some(day) {
            return;
        }
        self.last_finalized_day = Some(day);

        self.daily_closes.push_back((day, close_raw));
        while self
            .daily_closes
            .front()
            .is_some_and(|(old_day, _)| day - *old_day >= TRAILING_DAYS)
        {
            self.daily_closes.pop_front();
        }

        let trailing_high = self
            .daily_closes
            .iter()
            .map(|(_, close)| *close)
            .max()
            .unwrap_or(close_raw);

        let near_high = i128::from(close_raw) * 100 >= i128::from(trailing_high) * 98;
        if near_high {
            self.consecutive_near_high = self.consecutive_near_high.saturating_add(1);
        } else {
            self.consecutive_near_high = 0;
        }

        if ctx.open_orders() != 0 {
            return;
        }

        let position = ctx.position(INSTRUMENT);
        let at_drawdown_brake = i128::from(close_raw) * 100 <= i128::from(trailing_high) * 94;
        if position.is_positive() && at_drawdown_brake {
            ctx.submit(OrderRequest::market(INSTRUMENT, Side::Sell, position));
        } else if position.is_zero() && self.consecutive_near_high >= 2 {
            let qty = ctx.affordable_qty(
                INSTRUMENT,
                sizing_price,
                ctx.cash(),
                SIZING_MARGIN_PPM,
            );
            if qty.is_positive() {
                ctx.submit(OrderRequest::market(INSTRUMENT, Side::Buy, qty));
            }
        }
    }
}

impl Strategy for PeakDrawdownBrake {
    fn name(&self) -> String {
        "Peak-Drawdown Brake".to_string()
    }

    fn on_bar(&mut self, ctx: &mut Ctx, bar: &Bar) {
        if bar.instrument != INSTRUMENT {
            return;
        }

        let day_ns = TimeDelta::from_days(1).as_nanos();
        let minute_ns = TimeDelta::from_mins(1).as_nanos();
        let timestamp_ns = bar.close_ts.as_nanos();
        let day = timestamp_ns.div_euclid(day_ns);
        let minute_of_day = timestamp_ns.div_euclid(minute_ns).rem_euclid(1440);

        match self.active_day {
            Some(active_day) if day < active_day => return,
            Some(active_day) if day > active_day => {
                if let Some(close_raw) = self.active_close_raw {
                    self.finalize_day(active_day, close_raw, ctx, bar.close);
                }
                self.active_day = Some(day);
                self.active_close_raw = Some(bar.close.raw());
            }
            Some(_) => {
                self.active_close_raw = Some(bar.close.raw());
            }
            None => {
                self.active_day = Some(day);
                self.active_close_raw = Some(bar.close.raw());
            }
        }

        // When the final minute of a UTC day is present, its close is known
        // immediately, allowing a market order to target the following bar's open.
        if minute_of_day == 1439 {
            self.finalize_day(day, bar.close.raw(), ctx, bar.close);
        }
    }
}

export_strategy!(PeakDrawdownBrake {
    active_day: None,
    active_close_raw: None,
    last_finalized_day: None,
    daily_closes: VecDeque::new(),
    consecutive_near_high: 0,
});
